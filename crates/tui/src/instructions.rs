use std::path::{Path, PathBuf};

const FILENAME: &str = "AGENTS.md";

fn paths(config_dir: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    let root = config_dir.join(FILENAME);
    if root.exists() {
        paths.push(root);
    }

    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let candidate = d.join(FILENAME);
        if candidate.exists() {
            if paths.first().is_none_or(|r| *r != candidate) {
                paths.push(candidate);
            }
            break;
        }
        dir = d.parent();
    }

    paths
}

/// Returns all AGENTS.md files joined for the system prompt, or `None` if none found.
pub fn load(config_dir: &Path, cwd: &Path) -> Option<String> {
    let files = paths(config_dir, cwd);
    let pairs: Vec<(PathBuf, String)> = files
        .into_iter()
        .filter_map(|p| std::fs::read_to_string(&p).ok().map(|c| (p, c)))
        .collect();
    render_sections(&pairs)
}

/// Render a set of `(path, content)` pairs into the system-prompt block.
///
/// Each non-blank file becomes one `Instructions from <path>:\n<content>`
/// section; sections are joined by `\n\n`. Returns `None` when nothing
/// usable remains after trimming.
fn render_sections(files: &[(PathBuf, String)]) -> Option<String> {
    let sections: Vec<String> = files
        .iter()
        .filter_map(|(path, content)| {
            let trimmed = content.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(format!(
                    "Instructions from {}:\n{}",
                    path.display(),
                    trimmed
                ))
            }
        })
        .collect();
    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(p: &str, c: &str) -> (PathBuf, String) {
        (PathBuf::from(p), c.to_string())
    }

    #[test]
    fn render_sections_returns_none_for_empty_input() {
        assert_eq!(render_sections(&[]), None);
    }

    #[test]
    fn render_sections_returns_none_when_every_file_is_blank() {
        let files = vec![
            pair("/a/AGENTS.md", "   \n\t  \n"),
            pair("/b/AGENTS.md", ""),
        ];
        assert_eq!(render_sections(&files), None);
    }

    #[test]
    fn render_sections_renders_a_single_file_with_path_header() {
        let files = vec![pair("/work/AGENTS.md", "be terse\n")];
        let expected = "Instructions from /work/AGENTS.md:\nbe terse";
        assert_eq!(render_sections(&files).as_deref(), Some(expected));
    }

    #[test]
    fn render_sections_joins_multiple_files_with_double_newline() {
        let files = vec![
            pair("/cfg/AGENTS.md", "global rule"),
            pair("/proj/AGENTS.md", "project rule"),
        ];
        let out = render_sections(&files).unwrap();
        let expected = "Instructions from /cfg/AGENTS.md:\nglobal rule\n\n\
                        Instructions from /proj/AGENTS.md:\nproject rule";
        assert_eq!(out, expected);
    }

    #[test]
    fn render_sections_skips_blank_files_but_keeps_others() {
        let files = vec![
            pair("/a/AGENTS.md", "  "),
            pair("/b/AGENTS.md", "keep me"),
            pair("/c/AGENTS.md", ""),
        ];
        let out = render_sections(&files).unwrap();
        // Only `/b/AGENTS.md` survives; no leading/trailing separators.
        assert_eq!(out, "Instructions from /b/AGENTS.md:\nkeep me");
    }

    #[test]
    fn render_sections_trims_surrounding_whitespace_from_each_content() {
        let files = vec![pair("/x/AGENTS.md", "\n\n  hello\nworld  \n\n")];
        let out = render_sections(&files).unwrap();
        assert_eq!(out, "Instructions from /x/AGENTS.md:\nhello\nworld");
    }
}
