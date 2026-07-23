use std::collections::HashMap;
use std::path::{Path, PathBuf};

struct SkillFrontmatter {
    name: String,
    description: String,
}

#[derive(Debug, Clone)]
pub struct SkillShadowInfo {
    pub source: SkillSource,
    pub location: String,
}

#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub source: SkillSource,
    pub location: String,
    pub shadowed: Vec<SkillShadowInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillSource {
    Builtin,
    Skill,
    Command,
}

impl SkillSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::Skill => "skill",
            Self::Command => "command",
        }
    }
}

#[derive(Debug, Clone)]
struct SkillShadow {
    source: SkillSource,
    location: String,
}

#[derive(Debug, Clone)]
struct SkillEntry {
    name: String,
    description: String,
    source: SkillSource,
    location: String,
    formatted: String,
    shadowed: Vec<SkillShadow>,
}

/// Built-in skills shipped with smelt. Embedded at compile time and seeded
/// at lowest precedence so user/project copies with the same name win.
///
/// Bundled files (anything other than `SKILL.md` in the skill dir) are not
/// supported for built-ins - there's no real on-disk path to point the
/// agent at. If a built-in needs ancillary content, inline it into the
/// SKILL.md body.
static BUILTIN_SKILLS: &[(&str, &str)] = &[
    (
        "brief",
        include_str!("../../../runtime/skills/brief/SKILL.md"),
    ),
    (
        "customize",
        include_str!("../../../runtime/skills/customize/SKILL.md"),
    ),
    (
        "handoff",
        include_str!("../../../runtime/skills/handoff/SKILL.md"),
    ),
    (
        "reflect",
        include_str!("../../../runtime/skills/reflect/SKILL.md"),
    ),
    (
        "simplify",
        include_str!("../../../runtime/skills/simplify/SKILL.md"),
    ),
];

#[derive(Debug, Clone)]
pub struct SkillLoader {
    skills: HashMap<String, SkillEntry>,
    prompt_section: Option<String>,
}

impl SkillLoader {
    /// Load skills from built-ins, global, project-local, and extra
    /// directories. Later sources override earlier ones, so user skills
    /// can shadow built-ins by sharing the same `name:` in frontmatter.
    pub fn load(extra_paths: &[PathBuf]) -> Self {
        let home = crate::home_dir();
        let config_dir = crate::config_dir();
        let data_dir = crate::data_dir();
        let cwd = std::env::current_dir().ok();
        Self::load_from_paths(extra_paths, &home, &config_dir, &data_dir, cwd.as_deref())
    }

    pub fn load_for_runtime(
        extra_paths: &[PathBuf],
        home: &Path,
        config_dir: &Path,
        data_dir: &Path,
        cwd: &Path,
    ) -> Self {
        Self::load_from_paths(extra_paths, home, config_dir, data_dir, Some(cwd))
    }

    fn load_from_paths(
        extra_paths: &[PathBuf],
        home: &Path,
        config_dir: &Path,
        data_dir: &Path,
        cwd: Option<&Path>,
    ) -> Self {
        let mut skills = HashMap::new();

        for (name, body) in BUILTIN_SKILLS {
            let location = builtin_skill_path(data_dir, name).display().to_string();
            match parse_skill_text(body, None, &location, SkillSource::Builtin) {
                Some(entry) => {
                    insert_skill(&mut skills, entry);
                }
                None => {
                    eprintln!("smelt: built-in skill `{name}` failed to parse");
                }
            }
        }

        let global = config_dir.join("skills");
        scan_dir(&global, &mut skills);
        scan_command_dir(&config_dir.join("commands"), &mut skills);

        scan_dir(&home.join(".claude/skills"), &mut skills);
        scan_dir(&home.join(".agents/skills"), &mut skills);

        if let Some(cwd) = cwd {
            scan_dir(&cwd.join(".smelt/skills"), &mut skills);
            scan_command_dir(&cwd.join(".smelt/commands"), &mut skills);
            scan_dir(&cwd.join(".claude/skills"), &mut skills);
            scan_dir(&cwd.join(".agents/skills"), &mut skills);
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

    pub fn info(&self) -> Vec<SkillInfo> {
        let mut out: Vec<SkillInfo> = self
            .skills
            .values()
            .map(|entry| SkillInfo {
                name: entry.name.clone(),
                description: entry.description.clone(),
                source: entry.source,
                location: entry.location.clone(),
                shadowed: entry
                    .shadowed
                    .iter()
                    .map(|shadow| SkillShadowInfo {
                        source: shadow.source,
                        location: shadow.location.clone(),
                    })
                    .collect(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn prompt_section(&self) -> Option<&str> {
        self.prompt_section.as_deref()
    }
}

fn builtin_skill_path(data_dir: &Path, name: &str) -> PathBuf {
    data_dir
        .join("builtins")
        .join("skills")
        .join(name)
        .join("SKILL.md")
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

fn insert_skill(skills: &mut HashMap<String, SkillEntry>, mut entry: SkillEntry) {
    if let Some(previous) = skills.remove(&entry.name) {
        let mut shadowed = previous.shadowed;
        shadowed.push(SkillShadow {
            source: previous.source,
            location: previous.location,
        });
        entry.shadowed = shadowed;
    }
    skills.insert(entry.name.clone(), entry);
}

fn sorted_dir_entries(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
    paths.sort();
    paths
}

fn scan_dir(dir: &Path, skills: &mut HashMap<String, SkillEntry>) {
    for path in sorted_dir_entries(dir) {
        if !path.is_dir() {
            continue;
        }
        let skill_file = path.join("SKILL.md");
        if !skill_file.is_file() {
            continue;
        }
        if let Some(entry) = parse_skill(&skill_file) {
            insert_skill(skills, entry);
        }
    }
}

fn scan_command_dir(dir: &Path, skills: &mut HashMap<String, SkillEntry>) {
    for path in sorted_dir_entries(dir) {
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        if let Some(entry) = parse_command_skill(&path) {
            insert_skill(skills, entry);
        }
    }
}

fn xml_escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn parse_command_skill(path: &Path) -> Option<SkillEntry> {
    let text = std::fs::read_to_string(path).ok()?;
    let (fm, body) = split_frontmatter(&text)?;
    if !frontmatter_bool(fm, &["agent_skill", "agent-skill"]) {
        return None;
    }
    let name = path.file_stem()?.to_string_lossy().to_string();
    if name.is_empty() || name.contains(['/', '.']) {
        return None;
    }
    let description = frontmatter_string(fm, "description")
        .or_else(|| first_nonempty_line(body).map(trim_description))
        .unwrap_or_default();
    let location = path.display().to_string();
    let body = body.trim_start();
    let name_attr = xml_escape_attr(&name);
    let formatted = format!(
        "<skill name=\"{name_attr}\" included_by=\"smelt\" source=\"custom_command\">\n{body}\n\n## Slash command\n\nThis skill is also available to users as `/{name}`. When loaded as a skill, it is static, receives no slash-command arguments, and does not evaluate shell output markers.\n</skill>"
    );
    Some(SkillEntry {
        name,
        description,
        source: SkillSource::Command,
        location,
        formatted,
        shadowed: Vec::new(),
    })
}

fn parse_skill(path: &Path) -> Option<SkillEntry> {
    let text = std::fs::read_to_string(path).ok()?;
    let location = path.display().to_string();
    parse_skill_text(&text, path.parent(), &location, SkillSource::Skill)
}

/// Parse a `SKILL.md` body into a [`SkillEntry`]. `dir` is the on-disk
/// directory the skill came from - used to enumerate bundled files. Pass
/// `None` for built-ins, which have no on-disk base directory.
fn parse_skill_text(
    text: &str,
    dir: Option<&Path>,
    location: &str,
    source: SkillSource,
) -> Option<SkillEntry> {
    let (fm, body) = split_frontmatter(text)?;
    let meta = parse_frontmatter(fm)?;

    let name_attr = xml_escape_attr(&meta.name);
    let source_attr = source.as_str();
    let mut formatted = format!(
        "<skill name=\"{name_attr}\" included_by=\"smelt\" source=\"{source_attr}\">\n{body}"
    );

    if let Some(dir) = dir {
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
        source,
        location: location.to_string(),
        formatted,
        shadowed: Vec::new(),
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
    let mut reading_description = false;

    for raw_line in yaml.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        if let Some(rest) = line.strip_prefix("name:") {
            name = Some(unquote_yaml(rest.trim()));
            reading_description = false;
            continue;
        }

        if let Some(rest) = line.strip_prefix("description:") {
            description = unquote_yaml(rest.trim());
            reading_description = true;
            continue;
        }

        if reading_description && raw_line.starts_with(char::is_whitespace) {
            let continuation = unquote_yaml(line);
            if !continuation.is_empty() {
                if !description.is_empty() {
                    description.push(' ');
                }
                description.push_str(&continuation);
            }
            continue;
        }

        reading_description = false;
    }

    name.map(|name| SkillFrontmatter { name, description })
}

fn frontmatter_string(yaml: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    yaml.lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(&prefix).map(str::trim))
        .map(unquote_yaml)
        .filter(|s| !s.is_empty())
}

fn frontmatter_bool(yaml: &str, keys: &[&str]) -> bool {
    keys.iter().any(|key| {
        frontmatter_string(yaml, key).is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "true" | "yes" | "1"
            )
        })
    })
}

fn first_nonempty_line(body: &str) -> Option<&str> {
    body.lines().map(str::trim).find(|line| !line.is_empty())
}

fn trim_description(s: &str) -> String {
    const MAX_CHARS: usize = 80;
    let mut chars = s.chars();
    let out: String = chars.by_ref().take(MAX_CHARS).collect();
    if chars.next().is_some() {
        format!("{out}…")
    } else {
        out
    }
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
    fn parse_frontmatter_reads_indented_multiline_description() {
        let yaml =
            "name: customize\ndescription:\n  Change theme/colors, rebind keys,\n  write skills.";
        let fm = parse_frontmatter(yaml).unwrap();
        assert_eq!(fm.name, "customize");
        assert_eq!(
            fm.description,
            "Change theme/colors, rebind keys, write skills."
        );
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
                    source: SkillSource::Skill,
                    location: format!("/skills/{name}/SKILL.md"),
                    formatted: formatted.into(),
                    shadowed: Vec::new(),
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
    fn builtins_have_descriptions_and_real_locations() {
        let l = SkillLoader::load(&[]);
        let info = l.info();
        let customize = info.iter().find(|skill| skill.name == "customize").unwrap();
        assert!(customize.description.contains("Customize smelt"));
        assert!(customize
            .location
            .ends_with("builtins/skills/customize/SKILL.md"));

        let brief = info.iter().find(|skill| skill.name == "brief").unwrap();
        assert!(brief.description.contains("compact but exhaustive brief"));
        assert!(brief.location.ends_with("builtins/skills/brief/SKILL.md"));

        let handoff = info.iter().find(|skill| skill.name == "handoff").unwrap();
        assert!(handoff.description.contains("handoff summary"));
        assert!(handoff
            .location
            .ends_with("builtins/skills/handoff/SKILL.md"));

        let reflect = info.iter().find(|skill| skill.name == "reflect").unwrap();
        assert!(reflect.description.contains("Step back"));
        assert!(reflect
            .location
            .ends_with("builtins/skills/reflect/SKILL.md"));

        let simplify = info.iter().find(|skill| skill.name == "simplify").unwrap();
        assert!(simplify.description.contains("Review changed code"));
        assert!(simplify
            .location
            .ends_with("builtins/skills/simplify/SKILL.md"));
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
    fn info_returns_sorted_skill_metadata() {
        let l = loader_with(vec![("z", "Z desc", ""), ("a", "A desc", "")]);
        let info = l.info();
        assert_eq!(info[0].name, "a");
        assert_eq!(info[0].description, "A desc");
        assert_eq!(info[0].location, "/skills/a/SKILL.md");
        assert_eq!(info[1].name, "z");
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
    fn scan_command_dir_loads_opted_in_markdown_command_as_skill() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("review.md"),
            "---\ndescription: Review the current diff\nagent_skill: true\n---\n\nReview changed files.",
        );
        let mut map = HashMap::new();
        scan_command_dir(dir.path(), &mut map);
        let entry = map.get("review").unwrap();
        assert_eq!(entry.description, "Review the current diff");
        assert!(entry
            .formatted
            .contains("<skill name=\"review\" included_by=\"smelt\" source=\"custom_command\">"));
        assert!(entry.formatted.contains("Review changed files."));
        assert!(entry
            .formatted
            .contains("also available to users as `/review`"));
        assert!(entry
            .formatted
            .contains("does not evaluate shell output markers"));
    }

    #[test]
    fn scan_command_dir_skips_commands_without_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("private.md"),
            "---\ndescription: private\n---\n\nDo private things.",
        );
        let mut map = HashMap::new();
        scan_command_dir(dir.path(), &mut map);
        assert!(map.is_empty());
    }

    #[test]
    fn scan_command_dir_uses_body_for_missing_description() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("body-desc.md"),
            "---\nagent-skill: yes\n---\n\nFirst useful line.\n\nMore detail.",
        );
        let mut map = HashMap::new();
        scan_command_dir(dir.path(), &mut map);
        assert_eq!(map["body-desc"].description, "First useful line.");
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
        assert_eq!(
            entry.location,
            skill_dir.join("SKILL.md").display().to_string()
        );
        assert!(entry
            .formatted
            .contains("<skill name=\"my\" included_by=\"smelt\" source=\"skill\">"));
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
    fn later_skill_shadows_earlier_skill_and_records_source() {
        let mut map = HashMap::new();
        insert_skill(
            &mut map,
            SkillEntry {
                name: "same".into(),
                description: "old".into(),
                source: SkillSource::Builtin,
                location: "/builtin/SKILL.md".into(),
                formatted: "old body".into(),
                shadowed: Vec::new(),
            },
        );
        insert_skill(
            &mut map,
            SkillEntry {
                name: "same".into(),
                description: "new".into(),
                source: SkillSource::Skill,
                location: "/user/SKILL.md".into(),
                formatted: "new body".into(),
                shadowed: Vec::new(),
            },
        );

        let entry = map.get("same").unwrap();
        assert_eq!(entry.description, "new");
        assert_eq!(entry.source, SkillSource::Skill);
        assert_eq!(entry.shadowed.len(), 1);
        assert_eq!(entry.shadowed[0].source, SkillSource::Builtin);
        assert_eq!(entry.shadowed[0].location, "/builtin/SKILL.md");
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
