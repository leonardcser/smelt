use std::collections::HashMap;
use std::path::{Path, PathBuf};

struct SkillFrontmatter {
    name: String,
    description: String,
}

/// A discovered skill with its pre-formatted content for the LLM.
#[derive(Debug, Clone)]
struct SkillEntry {
    name: String,
    description: String,
    /// Pre-built content string returned by `content()`.
    formatted: String,
}

/// Loads and caches skills from well-known directories.
#[derive(Debug, Clone)]
pub struct SkillLoader {
    skills: HashMap<String, SkillEntry>,
    /// Pre-built system prompt section (computed once at load time).
    prompt_section: Option<String>,
}

impl SkillLoader {
    /// Scan all skill directories and load SKILL.md files.
    /// Directories searched (later entries override earlier ones):
    ///   1. ~/.config/smelt/skills/*/SKILL.md
    ///   2. .smelt/skills/*/SKILL.md (project-local)
    ///   3. Any extra paths from config
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

    /// Get skill content wrapped in tags for the LLM.
    /// Returns `Ok(content)` if found, `Err(message)` if not.
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

    /// List loaded skill names alphabetically. Used by the Lua
    /// `smelt.skills.list()` binding for plugins that want to enumerate.
    pub fn names(&self) -> Vec<String> {
        let mut out: Vec<String> = self.skills.keys().cloned().collect();
        out.sort();
        out
    }

    /// Pre-built system prompt section listing available skills.
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

    // Pre-build the formatted output
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

/// Split a markdown file into (frontmatter_yaml, body) at `---` delimiters.
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

/// Parse a minimal YAML frontmatter block: only `name` and `description`
/// keys are recognised. Values may be unquoted, double-quoted, or
/// single-quoted strings.
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

/// List non-SKILL.md files in a skill directory (up to 10).
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
}
