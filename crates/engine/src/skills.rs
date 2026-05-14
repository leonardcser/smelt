use std::collections::HashMap;
use std::path::{Path, PathBuf};

struct SkillFrontmatter {
    name: String,
    description: String,
}

#[derive(Debug, Clone)]
struct SkillEntry {
    name: String,
    description: String,
    formatted: String,
}

#[derive(Debug, Clone)]
pub struct SkillLoader {
    skills: HashMap<String, SkillEntry>,
    prompt_section: Option<String>,
}

impl SkillLoader {
    /// Load skills from global, project-local, and extra directories. Later entries override earlier ones.
    pub fn load(extra_paths: &[PathBuf]) -> Self {
        let mut skills = HashMap::new();

        let global = crate::config_dir().join("skills");
        scan_dir(&global, &mut skills);

        if let Ok(cwd) = std::env::current_dir() {
            scan_dir(&cwd.join(".smelt/skills"), &mut skills);
        }

        for path in extra_paths {
            scan_dir(path, &mut skills);
        }

        let prompt_section = build_prompt_section(&skills);
        Self {
            skills,
            prompt_section,
        }
    }

    pub fn content(&self, name: &str) -> Result<String, String> {
        match self.skills.get(name) {
            Some(entry) => Ok(entry.formatted.clone()),
            None => {
                let available: Vec<&str> = self.skills.keys().map(|s| s.as_str()).collect();
                Err(format!(
                    "Skill '{}' not found. Available skills: {}",
                    name,
                    if available.is_empty() {
                        "none".to_string()
                    } else {
                        available.join(", ")
                    }
                ))
            }
        }
    }

    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = self.skills.keys().cloned().collect();
        out.sort();
        out
    }

    pub fn prompt_section(&self) -> Option<&str> {
        self.prompt_section.as_deref()
    }
}

fn build_prompt_section(skills: &HashMap<String, SkillEntry>) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut lines = vec!["# Skills\n\nUse the `load_skill` tool to load specialized knowledge on demand.\n\nAvailable skills:".to_string()];
    let mut names: Vec<&String> = skills.keys().collect();
    names.sort();
    for name in names {
        let skill = &skills[name];
        lines.push(format!("  - {}: {}", skill.name, skill.description));
    }
    Some(lines.join("\n"))
}

fn scan_dir(dir: &Path, skills: &mut HashMap<String, SkillEntry>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_file = path.join("SKILL.md");
        if !skill_file.is_file() {
            continue;
        }
        if let Some(entry) = parse_skill(&skill_file) {
            skills.insert(entry.name.clone(), entry);
        }
    }
}

fn parse_skill(path: &Path) -> Option<SkillEntry> {
    let text = std::fs::read_to_string(path).ok()?;
    let (fm, body) = split_frontmatter(&text)?;
    let meta = parse_frontmatter(fm)?;

    let mut formatted = format!("<skill name=\"{}\">\n{}", meta.name, body);

    if let Some(dir) = path.parent() {
        let files = list_bundled_files(dir);
        if !files.is_empty() {
            formatted.push_str("\n\n## Bundled files\n");
            for f in &files {
                formatted.push_str(&format!("- {}\n", f));
            }
            formatted.push_str(&format!("\nBase directory: {}\n", dir.display()));
        }
    }

    formatted.push_str("\n</skill>");

    Some(SkillEntry {
        name: meta.name,
        description: meta.description,
        formatted,
    })
}

fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let text = text.trim_start();
    if !text.starts_with("---") {
        return None;
    }
    let after_first = &text[3..];
    let end = after_first.find("\n---")?;
    let yaml = after_first[..end].trim();
    let body = after_first[end + 4..].trim_start();
    Some((yaml, body))
}

/// Parse `name` and `description` from a minimal YAML frontmatter block.
fn parse_frontmatter(yaml: &str) -> Option<SkillFrontmatter> {
    let mut name = None;
    let mut description = String::new();
    for line in yaml.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("name:") {
            name = Some(unquote_yaml(rest.trim()));
        } else if let Some(rest) = line.strip_prefix("description:") {
            description = unquote_yaml(rest.trim());
        }
    }
    name.map(|name| SkillFrontmatter { name, description })
}

fn unquote_yaml(s: &str) -> String {
    if s.len() >= 2 {
        let bytes = s.as_bytes();
        let first = bytes[0] as char;
        let last = bytes[bytes.len() - 1] as char;
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

fn list_bundled_files(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name == "SKILL.md" {
            continue;
        }
        if path.is_dir() {
            files.push(format!("{}/", name));
        } else {
            files.push(name);
        }
        if files.len() >= 10 {
            break;
        }
    }
    files.sort();
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_frontmatter_basic() {
        let text = "---\nname: test\ndescription: A test\n---\n\nbody here";
        let (yaml, body) = split_frontmatter(text).unwrap();
        assert!(yaml.contains("name: test"));
        assert!(body.contains("body here"));
    }

    #[test]
    fn split_frontmatter_missing() {
        assert!(split_frontmatter("no frontmatter here").is_none());
    }

    #[test]
    fn parse_frontmatter_with_serde() {
        let yaml = "name: test-skill\ndescription: A test skill";
        let fm = parse_frontmatter(yaml).unwrap();
        assert_eq!(fm.name, "test-skill");
        assert_eq!(fm.description, "A test skill");
    }

    #[test]
    fn parse_frontmatter_quoted() {
        let yaml = "name: \"quoted-name\"\ndescription: 'quoted desc'";
        let fm = parse_frontmatter(yaml).unwrap();
        assert_eq!(fm.name, "quoted-name");
        assert_eq!(fm.description, "quoted desc");
    }

    #[test]
    fn split_frontmatter_returns_none_when_only_opening_delimiter() {
        assert!(split_frontmatter("---\nname: x\nbody no closer").is_none());
    }

    #[test]
    fn split_frontmatter_trims_leading_whitespace_before_delimiter() {
        let (yaml, body) = split_frontmatter("\n   \n---\nname: x\n---\nbody").unwrap();
        assert!(yaml.contains("name: x"));
        assert_eq!(body, "body");
    }

    #[test]
    fn parse_frontmatter_returns_none_when_name_missing() {
        let fm = parse_frontmatter("description: only");
        assert!(fm.is_none());
    }

    #[test]
    fn parse_frontmatter_description_defaults_to_empty_when_absent() {
        let fm = parse_frontmatter("name: only").unwrap();
        assert_eq!(fm.name, "only");
        assert_eq!(fm.description, "");
    }

    #[test]
    fn parse_frontmatter_skips_blank_lines() {
        let fm = parse_frontmatter("\nname: a\n\ndescription: b\n").unwrap();
        assert_eq!(fm.name, "a");
        assert_eq!(fm.description, "b");
    }

    #[test]
    fn unquote_yaml_strips_matched_quotes() {
        assert_eq!(unquote_yaml("\"x\""), "x");
        assert_eq!(unquote_yaml("'x'"), "x");
    }

    #[test]
    fn unquote_yaml_leaves_unquoted_or_mismatched_intact() {
        assert_eq!(unquote_yaml("plain"), "plain");
        assert_eq!(unquote_yaml("\"mismatched'"), "\"mismatched'");
        assert_eq!(unquote_yaml(""), "");
        assert_eq!(unquote_yaml("\""), "\"");
    }

    // ---- SkillLoader (constructed directly to bypass dir scanning) ----

    fn loader_with(entries: Vec<(&str, &str, &str)>) -> SkillLoader {
        let mut skills = HashMap::new();
        for (name, description, formatted) in entries {
            skills.insert(
                name.to_string(),
                SkillEntry {
                    name: name.into(),
                    description: description.into(),
                    formatted: formatted.into(),
                },
            );
        }
        let prompt_section = build_prompt_section(&skills);
        SkillLoader {
            skills,
            prompt_section,
        }
    }

    #[test]
    fn content_returns_formatted_text_for_known_skill() {
        let l = loader_with(vec![("a", "desc", "<skill>a body</skill>")]);
        assert_eq!(l.content("a").unwrap(), "<skill>a body</skill>");
    }

    #[test]
    fn content_returns_err_listing_available_when_unknown() {
        let l = loader_with(vec![("a", "d", "x"), ("b", "d", "x")]);
        let err = l.content("missing").unwrap_err();
        assert!(err.contains("'missing'"));
        assert!(err.contains("a"));
        assert!(err.contains("b"));
    }

    #[test]
    fn content_lists_none_when_no_skills_available() {
        let l = loader_with(vec![]);
        let err = l.content("missing").unwrap_err();
        assert!(err.contains("none"));
    }

    #[test]
    fn names_returns_sorted_skill_names() {
        let l = loader_with(vec![("z", "", ""), ("a", "", ""), ("m", "", "")]);
        assert_eq!(l.names(), vec!["a", "m", "z"]);
    }

    #[test]
    fn prompt_section_is_none_when_no_skills_loaded() {
        let l = loader_with(vec![]);
        assert!(l.prompt_section().is_none());
    }

    #[test]
    fn prompt_section_lists_skills_with_descriptions_sorted() {
        let l = loader_with(vec![("z", "Z desc", ""), ("a", "A desc", "")]);
        let p = l.prompt_section().unwrap();
        assert!(p.contains("# Skills"));
        let a_pos = p.find("a: A desc").unwrap();
        let z_pos = p.find("z: Z desc").unwrap();
        assert!(a_pos < z_pos);
    }

    // ---- scan_dir / parse_skill via tempdir ----

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn scan_dir_loads_skill_from_skill_md_with_valid_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("mySkill");
        write(
            &skill_dir.join("SKILL.md"),
            "---\nname: my\ndescription: cool\n---\n\nuse me wisely",
        );
        let mut map = HashMap::new();
        scan_dir(dir.path(), &mut map);
        assert!(map.contains_key("my"));
        let entry = &map["my"];
        assert_eq!(entry.description, "cool");
        assert!(entry.formatted.contains("<skill name=\"my\">"));
        assert!(entry.formatted.contains("use me wisely"));
        assert!(entry.formatted.ends_with("</skill>"));
    }

    #[test]
    fn scan_dir_skips_directories_without_skill_md() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("bare")).unwrap();
        let mut map = HashMap::new();
        scan_dir(dir.path(), &mut map);
        assert!(map.is_empty());
    }

    #[test]
    fn scan_dir_skips_invalid_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("nope/SKILL.md"), "no frontmatter here");
        let mut map = HashMap::new();
        scan_dir(dir.path(), &mut map);
        assert!(map.is_empty());
    }

    #[test]
    fn scan_dir_is_silent_for_missing_directory() {
        let mut map = HashMap::new();
        scan_dir(Path::new("/nonexistent/path/xyzzy"), &mut map);
        assert!(map.is_empty());
    }

    #[test]
    fn parse_skill_lists_bundled_files() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("my");
        write(
            &skill_dir.join("SKILL.md"),
            "---\nname: my\ndescription: d\n---\n\nbody",
        );
        write(&skill_dir.join("helper.sh"), "#!/bin/bash\n");
        std::fs::create_dir_all(skill_dir.join("subdir")).unwrap();

        let entry = parse_skill(&skill_dir.join("SKILL.md")).unwrap();
        assert!(entry.formatted.contains("## Bundled files"));
        assert!(entry.formatted.contains("- helper.sh"));
        assert!(entry.formatted.contains("- subdir/"));
        assert!(entry.formatted.contains("Base directory:"));
    }

    #[test]
    fn list_bundled_files_caps_at_ten_entries() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..15 {
            write(&dir.path().join(format!("f{i}.txt")), "x");
        }
        let files = list_bundled_files(dir.path());
        assert_eq!(files.len(), 10);
    }

    #[test]
    fn list_bundled_files_excludes_skill_md() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("SKILL.md"), "x");
        write(&dir.path().join("other.txt"), "x");
        let files = list_bundled_files(dir.path());
        assert_eq!(files, vec!["other.txt"]);
    }

    #[test]
    fn list_bundled_files_handles_missing_directory() {
        assert!(list_bundled_files(Path::new("/no/such/dir/xyz")).is_empty());
    }
}
