//! Materialize a tag-derived agent version in a release runner checkout.

use semver::Version;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use toml_edit::{value, DocumentMut, Item, TableLike};

const DEPENDENCY_TABLES: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];

pub fn run(args: Vec<String>) {
    let [version] = args.as_slice() else {
        die("usage: cargo xtask prepare-release <version>");
    };
    let root = crate::repo_root();
    if let Err(error) = prepare(&root, version, true) {
        die(&error);
    }
}

fn prepare(root: &Path, version: &str, refresh_locks: bool) -> Result<(), String> {
    validate_version(version)?;
    let mut workspace = Workspace::load(root)?;
    workspace.materialize(version)?;
    workspace.write()?;
    if refresh_locks {
        refresh_lockfile(root, &root.join("Cargo.toml"))?;
        refresh_lockfile(root, &root.join("fuzz/Cargo.toml"))?;
    }
    println!(
        "prepared runner-local release version {version} across {} package(s)",
        workspace.managed_packages
    );
    Ok(())
}

fn validate_version(version: &str) -> Result<(), String> {
    let parsed =
        Version::parse(version).map_err(|error| format!("invalid release version: {error}"))?;
    if parsed.major == 0 && !parsed.pre.is_empty() {
        return Err(
            "0.x releases are beta-quality normal releases; remove the prerelease suffix".into(),
        );
    }
    Ok(())
}

struct Manifest {
    path: PathBuf,
    document: DocumentMut,
    package_name: Option<String>,
}

impl Manifest {
    fn load(path: PathBuf) -> Result<Self, String> {
        let source = std::fs::read_to_string(&path)
            .map_err(|error| format!("read {}: {error}", path.display()))?;
        let document = source
            .parse::<DocumentMut>()
            .map_err(|error| format!("parse {}: {error}", path.display()))?;
        let package_name = document
            .get("package")
            .and_then(Item::as_table_like)
            .and_then(|package| package.get("name"))
            .and_then(Item::as_str)
            .map(str::to_owned);
        Ok(Self {
            path,
            document,
            package_name,
        })
    }

    fn set_package_version(&mut self, version: &str) -> Result<(), String> {
        let package = self
            .document
            .get_mut("package")
            .and_then(Item::as_table_like_mut)
            .ok_or_else(|| format!("{} has no [package] table", self.path.display()))?;
        package.insert("version", value(version));
        Ok(())
    }

    fn update_path_dependencies(
        &mut self,
        versions_by_manifest: &HashMap<PathBuf, String>,
    ) -> Result<(), String> {
        let base = self
            .path
            .parent()
            .ok_or_else(|| format!("{} has no parent directory", self.path.display()))?;

        for section in DEPENDENCY_TABLES {
            if let Some(dependencies) = self
                .document
                .get_mut(section)
                .and_then(Item::as_table_like_mut)
            {
                update_dependency_table(dependencies, base, versions_by_manifest)?;
            }
        }

        if let Some(dependencies) = self
            .document
            .get_mut("workspace")
            .and_then(Item::as_table_like_mut)
            .and_then(|workspace| workspace.get_mut("dependencies"))
            .and_then(Item::as_table_like_mut)
        {
            update_dependency_table(dependencies, base, versions_by_manifest)?;
        }

        if let Some(targets) = self
            .document
            .get_mut("target")
            .and_then(Item::as_table_like_mut)
        {
            for (_, target) in targets.iter_mut() {
                let Some(target) = target.as_table_like_mut() else {
                    continue;
                };
                for section in DEPENDENCY_TABLES {
                    if let Some(dependencies) =
                        target.get_mut(section).and_then(Item::as_table_like_mut)
                    {
                        update_dependency_table(dependencies, base, versions_by_manifest)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn write(&self) -> Result<(), String> {
        std::fs::write(&self.path, self.document.to_string())
            .map_err(|error| format!("write {}: {error}", self.path.display()))
    }
}

struct Workspace {
    manifests: Vec<Manifest>,
    independent: HashSet<String>,
    managed_packages: usize,
}

impl Workspace {
    fn load(root: &Path) -> Result<Self, String> {
        let root_manifest_path = canonical_manifest(&root.join("Cargo.toml"))?;
        let root_manifest = Manifest::load(root_manifest_path)?;
        let workspace = root_manifest
            .document
            .get("workspace")
            .and_then(Item::as_table_like)
            .ok_or_else(|| "root Cargo.toml has no [workspace] table".to_string())?;
        let members = workspace
            .get("members")
            .and_then(Item::as_array)
            .ok_or_else(|| "root Cargo.toml has no workspace members".to_string())?
            .iter()
            .map(|member| {
                member
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "workspace members must be strings".to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let independent = root_manifest
            .document
            .get("workspace")
            .and_then(Item::as_table_like)
            .and_then(|workspace| workspace.get("metadata"))
            .and_then(Item::as_table_like)
            .and_then(|metadata| metadata.get("smelt"))
            .and_then(Item::as_table_like)
            .and_then(|smelt| smelt.get("release"))
            .and_then(Item::as_table_like)
            .and_then(|release| release.get("independent-crates"))
            .and_then(Item::as_array)
            .ok_or_else(|| {
                "root Cargo.toml has no workspace.metadata.smelt.release.independent-crates"
                    .to_string()
            })?
            .iter()
            .map(|name| {
                name.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "independent crate names must be strings".to_string())
            })
            .collect::<Result<HashSet<_>, _>>()?;

        let mut manifests = vec![root_manifest];
        for member in members {
            manifests.push(Manifest::load(canonical_manifest(
                &root.join(member).join("Cargo.toml"),
            )?)?);
        }
        let fuzz_manifest = root.join("fuzz/Cargo.toml");
        if fuzz_manifest.exists() {
            manifests.push(Manifest::load(canonical_manifest(&fuzz_manifest)?)?);
        }

        let mut names = HashSet::new();
        for manifest in &manifests {
            if let Some(name) = &manifest.package_name {
                if !names.insert(name.clone()) {
                    return Err(format!("duplicate package name `{name}`"));
                }
            }
        }
        for name in &independent {
            if !names.contains(name) {
                return Err(format!(
                    "independent crate `{name}` is not a workspace package"
                ));
            }
        }

        Ok(Self {
            manifests,
            independent,
            managed_packages: 0,
        })
    }

    fn materialize(&mut self, release_version: &str) -> Result<(), String> {
        let fuzz_manifest = self
            .manifests
            .iter()
            .position(|manifest| manifest.package_name.as_deref() == Some("smelt-fuzz"));
        let mut versions_by_manifest = HashMap::new();

        for (index, manifest) in self.manifests.iter_mut().enumerate() {
            let Some(name) = manifest.package_name.clone() else {
                continue;
            };
            let managed = Some(index) != fuzz_manifest && !self.independent.contains(&name);
            if managed {
                manifest.set_package_version(release_version)?;
                self.managed_packages += 1;
            }
            let version = manifest
                .document
                .get("package")
                .and_then(Item::as_table_like)
                .and_then(|package| package.get("version"))
                .and_then(Item::as_str)
                .ok_or_else(|| format!("package `{name}` has no version"))?;
            versions_by_manifest.insert(manifest.path.clone(), version.to_string());
        }

        for manifest in &mut self.manifests {
            manifest.update_path_dependencies(&versions_by_manifest)?;
        }
        Ok(())
    }

    fn write(&self) -> Result<(), String> {
        for manifest in &self.manifests {
            manifest.write()?;
        }
        Ok(())
    }
}

fn update_dependency_table(
    dependencies: &mut dyn TableLike,
    base: &Path,
    versions_by_manifest: &HashMap<PathBuf, String>,
) -> Result<(), String> {
    for (name, dependency) in dependencies.iter_mut() {
        let Some(path) = dependency_path(dependency) else {
            continue;
        };
        let manifest_path = canonical_manifest(&base.join(path).join("Cargo.toml"))?;
        let Some(version) = versions_by_manifest.get(&manifest_path) else {
            continue;
        };
        let dependency = dependency
            .as_table_like_mut()
            .ok_or_else(|| format!("path dependency `{name}` must be a table"))?;
        dependency.insert("version", value(version));
    }
    Ok(())
}

fn dependency_path(item: &Item) -> Option<&str> {
    item.as_table_like()
        .and_then(|dependency| dependency.get("path"))
        .and_then(Item::as_str)
}

fn canonical_manifest(path: &Path) -> Result<PathBuf, String> {
    path.canonicalize()
        .map_err(|error| format!("resolve {}: {error}", path.display()))
}

fn refresh_lockfile(root: &Path, manifest: &Path) -> Result<(), String> {
    let status = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(manifest)
        .current_dir(root)
        .stdout(Stdio::null())
        .status()
        .map_err(|error| format!("refresh lockfile for {}: {error}", manifest.display()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "cargo metadata failed while refreshing {}",
            manifest.display()
        ))
    }
}

fn die(message: &str) -> ! {
    eprintln!("xtask prepare-release: {message}");
    std::process::exit(2);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn materializes_managed_packages_and_internal_path_requirements() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(
            &root.join("Cargo.toml"),
            r#"[workspace]
members = ["crates/app", "crates/library"]

[workspace.metadata.smelt.release]
independent-crates = ["library"]

[workspace.dependencies]
app = { path = "crates/app" }
library = { path = "crates/library", version = "2.1.0" }

[package]
name = "agent"
version = "0.5.0-alpha.12"

[target.'cfg(unix)'.dev-dependencies]
app = { path = "crates/app" }
"#,
        );
        write(
            &root.join("crates/app/Cargo.toml"),
            r#"[package]
name = "app"
version = "0.5.0-alpha.12"

[dependencies]
library = { path = "../library" }

[build-dependencies.library]
path = "../library"
"#,
        );
        write(
            &root.join("crates/library/Cargo.toml"),
            r#"[package]
name = "library"
version = "2.1.0"
"#,
        );
        write(
            &root.join("fuzz/Cargo.toml"),
            r#"[workspace]

[package]
name = "smelt-fuzz"
version = "0.0.0"

[dependencies]
app = { path = "../crates/app" }
library = { path = "../crates/library" }
"#,
        );

        prepare(root, "0.6.0", false).unwrap();

        let root_doc = std::fs::read_to_string(root.join("Cargo.toml"))
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(root_doc["package"]["version"].as_str(), Some("0.6.0"));
        assert_eq!(
            root_doc["workspace"]["dependencies"]["app"]["version"].as_str(),
            Some("0.6.0")
        );
        assert_eq!(
            root_doc["workspace"]["dependencies"]["library"]["version"].as_str(),
            Some("2.1.0")
        );
        assert_eq!(
            root_doc["target"]["cfg(unix)"]["dev-dependencies"]["app"]["version"].as_str(),
            Some("0.6.0")
        );

        let app = std::fs::read_to_string(root.join("crates/app/Cargo.toml"))
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(app["package"]["version"].as_str(), Some("0.6.0"));
        assert_eq!(
            app["dependencies"]["library"]["version"].as_str(),
            Some("2.1.0")
        );
        assert_eq!(
            app["build-dependencies"]["library"]["version"].as_str(),
            Some("2.1.0")
        );

        let library = std::fs::read_to_string(root.join("crates/library/Cargo.toml"))
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(library["package"]["version"].as_str(), Some("2.1.0"));

        let fuzz = std::fs::read_to_string(root.join("fuzz/Cargo.toml"))
            .unwrap()
            .parse::<DocumentMut>()
            .unwrap();
        assert_eq!(fuzz["package"]["version"].as_str(), Some("0.0.0"));
        assert_eq!(
            fuzz["dependencies"]["app"]["version"].as_str(),
            Some("0.6.0")
        );
        assert_eq!(
            fuzz["dependencies"]["library"]["version"].as_str(),
            Some("2.1.0")
        );
    }

    #[test]
    fn rejects_prerelease_suffixes_for_beta_releases() {
        assert!(validate_version("0.6.0-beta.1")
            .unwrap_err()
            .contains("remove the prerelease suffix"));
        assert!(validate_version("0.6.0").is_ok());
    }
}
