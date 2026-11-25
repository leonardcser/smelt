use smelt_buffer::text;
use std::path::{Path, PathBuf};

const FILENAME: &str = "AGENTS.md";
/// Machine-local instructions kept out of version control. Read from the same
/// project folder as `AGENTS.md` and placed after it.
const LOCAL_FILENAME: &str = "AGENTS.local.md";

/// A linked Git worktree and its repository's main worktree. Git does not
/// copy ignored files into new worktrees, so a folder without its own
/// `AGENTS.local.md` reads the one in the same folder of the main worktree.
struct MainWorktree {
    linked_root: PathBuf,
    main_root: PathBuf,
}

impl MainWorktree {
    fn for_cwd(cwd: &Path) -> Option<Self> {
        let (linked_root, main_root) = smelt_worktree::linked_worktree_roots(cwd)?;
        Some(Self {
            linked_root,
            main_root,
        })
    }

    /// `dir` in the main worktree, or `None` outside the linked worktree.
    fn mirror(&self, dir: &Path) -> Option<PathBuf> {
        let dir = std::fs::canonicalize(dir).ok()?;
        let relative = dir.strip_prefix(&self.linked_root).ok()?;
        Some(self.main_root.join(relative))
    }
}

/// Global `AGENTS.md`, then `AGENTS.md` and `AGENTS.local.md` from the nearest
/// folder at or above `cwd` that has either file. In a linked worktree,
/// `AGENTS.local.md` may come from the same folder of the main worktree.
fn paths(config_dir: &Path, cwd: &Path, main: Option<&MainWorktree>) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    let root = config_dir.join(FILENAME);
    if root.exists() {
        paths.push(root);
    }

    let project = cwd.ancestors().find_map(|dir| {
        let shared = Some(dir.join(FILENAME)).filter(|path| path.exists());
        let local = std::iter::once(dir.to_path_buf())
            .chain(main.and_then(|main| main.mirror(dir)))
            .map(|dir| dir.join(LOCAL_FILENAME))
            .find(|path| path.exists());
        let found: Vec<PathBuf> = shared.into_iter().chain(local).collect();
        (!found.is_empty()).then_some(found)
    });
    for path in project.into_iter().flatten() {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }

    paths
}

/// Returns all instruction files joined for the system prompt, or `None` if none found.
pub fn load(config_dir: &Path, cwd: &Path) -> Option<String> {
    let files = paths(config_dir, cwd, MainWorktree::for_cwd(cwd).as_ref());
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
            let trimmed = text::trim_whitespace(content);
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

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x").unwrap();
    }

    #[test]
    fn paths_reads_local_file_after_project_file_in_same_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        let project = tmp.path().join("project");
        let cwd = project.join("src/deep");
        std::fs::create_dir_all(&cwd).unwrap();
        touch(&config.join(FILENAME));
        touch(&project.join(LOCAL_FILENAME));
        touch(&project.join(FILENAME));

        assert_eq!(
            paths(&config, &cwd, None),
            vec![
                config.join(FILENAME),
                project.join(FILENAME),
                project.join(LOCAL_FILENAME),
            ]
        );
    }

    #[test]
    fn paths_stops_at_nearest_folder_with_only_a_local_file() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        let outer = tmp.path().join("outer");
        let inner = outer.join("inner");
        touch(&outer.join(FILENAME));
        touch(&inner.join(LOCAL_FILENAME));

        assert_eq!(
            paths(&config, &inner, None),
            vec![inner.join(LOCAL_FILENAME)]
        );
    }

    fn worktrees() -> (tempfile::TempDir, PathBuf, MainWorktree) {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let main = MainWorktree {
            linked_root: root.join("linked"),
            main_root: root.join("main"),
        };
        std::fs::create_dir_all(main.linked_root.join("crates/x")).unwrap();
        (tmp, root.join("config"), main)
    }

    #[test]
    fn paths_in_linked_worktree_reads_local_file_from_main_worktree() {
        let (_tmp, config, main) = worktrees();
        touch(&main.linked_root.join(FILENAME));
        touch(&main.main_root.join(FILENAME));
        touch(&main.main_root.join(LOCAL_FILENAME));

        assert_eq!(
            paths(&config, &main.linked_root.join("crates/x"), Some(&main)),
            vec![
                main.linked_root.join(FILENAME),
                main.main_root.join(LOCAL_FILENAME),
            ]
        );
    }

    #[test]
    fn paths_in_linked_worktree_prefers_its_own_local_file() {
        let (_tmp, config, main) = worktrees();
        touch(&main.linked_root.join(FILENAME));
        touch(&main.linked_root.join(LOCAL_FILENAME));
        touch(&main.main_root.join(LOCAL_FILENAME));

        assert_eq!(
            paths(&config, &main.linked_root, Some(&main)),
            vec![
                main.linked_root.join(FILENAME),
                main.linked_root.join(LOCAL_FILENAME),
            ]
        );
    }

    #[test]
    fn paths_in_linked_worktree_mirrors_the_nearest_project_folder() {
        let (_tmp, config, main) = worktrees();
        touch(&main.linked_root.join(FILENAME));
        touch(&main.main_root.join("crates/x").join(LOCAL_FILENAME));

        assert_eq!(
            paths(&config, &main.linked_root.join("crates/x"), Some(&main)),
            vec![main.main_root.join("crates/x").join(LOCAL_FILENAME)]
        );
    }

    #[test]
    fn paths_lists_config_dir_files_once_when_cwd_is_config_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().to_path_buf();
        touch(&config.join(FILENAME));
        touch(&config.join(LOCAL_FILENAME));

        assert_eq!(
            paths(&config, &config, None),
            vec![config.join(FILENAME), config.join(LOCAL_FILENAME)]
        );
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

    #[test]
    fn render_sections_never_trims_part_of_a_grapheme() {
        let files = vec![pair("/x/AGENTS.md", " \u{301}rule\u{600} ")];
        let out = render_sections(&files).unwrap();
        assert_eq!(out, "Instructions from /x/AGENTS.md:\n \u{301}rule\u{600} ");
    }
}
