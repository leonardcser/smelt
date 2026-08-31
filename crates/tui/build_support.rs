use std::path::{Path, PathBuf};

#[derive(Debug)]
pub(crate) struct BuildIdentity {
    pub tag: String,
    pub commits: String,
    pub dirty: String,
    pub display: String,
}

pub(crate) fn resolve_identity(
    described: Option<&str>,
    pkg_version: &str,
    release_tag: Option<&str>,
) -> Result<BuildIdentity, String> {
    if let Some(release_tag) = release_tag {
        let expected = format!("v{pkg_version}");
        if release_tag != expected {
            return Err(format!(
                "SMELT_RELEASE_TAG `{release_tag}` does not match package version `{pkg_version}`"
            ));
        }
        return Ok(BuildIdentity {
            tag: release_tag.to_string(),
            commits: "0".into(),
            dirty: "0".into(),
            display: release_tag.to_string(),
        });
    }

    Ok(described
        .map(|described| parse_describe(described, pkg_version))
        .unwrap_or_else(|| BuildIdentity {
            tag: "unknown".into(),
            commits: "0".into(),
            dirty: "0".into(),
            display: format!("v{pkg_version}"),
        }))
}

fn parse_describe(described: &str, pkg_version: &str) -> BuildIdentity {
    let (core, dirty) = match described.strip_suffix("-dirty") {
        Some(rest) => (rest, "1"),
        None => (described, "0"),
    };
    // Tags may contain hyphens, so peel the SHA and commit distance from the right.
    let parts: Vec<&str> = core.rsplitn(3, '-').collect();
    let (tag, commits, sha) = if parts.len() == 3 && parts[0].starts_with('g') {
        (parts[2], parts[1], parts[0].trim_start_matches('g'))
    } else {
        return BuildIdentity {
            tag: "unknown".into(),
            commits: "0".into(),
            dirty: dirty.into(),
            display: format!("v{pkg_version}"),
        };
    };
    let display_tag = if tag.starts_with('v') {
        tag.to_string()
    } else {
        format!("v{tag}")
    };
    let display = if commits == "0" && dirty == "0" {
        display_tag
    } else {
        format!(
            "{display_tag}-{commits}-{sha}{}",
            if dirty == "1" { "-dirty" } else { "" }
        )
    };
    BuildIdentity {
        tag: tag.into(),
        commits: commits.into(),
        dirty: dirty.into(),
        display,
    }
}

pub(crate) fn git_pathspecs(head_ref: Option<&str>) -> Vec<&str> {
    let mut pathspecs = vec!["HEAD"];
    if let Some(head_ref) = head_ref {
        if !head_ref.is_empty() && head_ref != "HEAD" {
            pathspecs.push(head_ref);
        }
    }
    pathspecs.extend(["index", "refs/tags", "packed-refs"]);
    pathspecs
}

pub(crate) fn tracked_file_paths(repo_root: &Path, git_ls_files: &str) -> Vec<PathBuf> {
    git_ls_files
        .lines()
        .filter(|path| !path.is_empty())
        .map(|path| repo_root.join(path))
        .collect()
}
