//! Read-only revision comparisons and bounded commit pages.

use super::*;

#[derive(Debug)]
pub struct Commit {
    pub id: String,
    pub author: String,
    pub age: String,
    pub subject: String,
    pub message: String,
    pub additions: u64,
    pub deletions: u64,
    pub binary: bool,
}

#[derive(Debug)]
pub struct Page {
    pub commits: Vec<Commit>,
    pub base: Option<String>,
    pub base_id: Option<String>,
    pub head: Option<String>,
    pub more: bool,
}

async fn resolve(
    root: &Path,
    revision: &str,
    cancel: &CancellationToken,
) -> Result<String, String> {
    let output = checked(
        git(
            root,
            &args(&[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{revision}^{{commit}}"),
            ]),
            cancel,
        )
        .await?,
    )?;
    Ok(String::from_utf8_lossy(&output).trim().to_string())
}

async fn base_ref(
    root: &Path,
    base: Option<&str>,
    cancel: &CancellationToken,
) -> Result<(String, String), String> {
    if let Some(base) = base {
        // A same-named tag must not redirect the branch comparison on refresh.
        let id = match resolve(root, &format!("refs/heads/{base}"), cancel).await {
            Ok(id) => id,
            Err(_) => resolve(root, base, cancel).await?,
        };
        return Ok((base.into(), id));
    }
    for name in ["main", "master"] {
        if let Ok(id) = resolve(root, &format!("refs/heads/{name}"), cancel).await {
            return Ok((name.into(), id));
        }
    }
    Err("choose a comparison base with Alt-b (no main or master branch)".into())
}

/// Commits per history page.
const PAGE_SIZE: usize = 50;

pub async fn history(
    root: PathBuf,
    base: Option<String>,
    branch: bool,
    head: Option<String>,
    skip: usize,
    cancel: CancellationToken,
) -> Result<Page, String> {
    tokio::time::timeout(Duration::from_secs(120), async {
        let head = match head {
            Some(head) => resolve(&root, &head, &cancel).await?,
            None => match resolve(&root, "HEAD", &cancel).await {
                Ok(head) => head,
                Err(_) => {
                    // Distinguish a new repository from a missing repository or broken ref.
                    checked(git(&root, &args(&["rev-parse", "--git-dir"]), &cancel).await?)?;
                    let symbolic =
                        git(&root, &args(&["symbolic-ref", "--quiet", "HEAD"]), &cancel).await?;
                    if !symbolic.status.success() {
                        return Err("cannot resolve HEAD".into());
                    }
                    return Ok(Page {
                        commits: Vec::new(),
                        base,
                        base_id: None,
                        head: None,
                        more: false,
                    });
                }
            },
        };
        let (base, base_id) = if branch {
            let (name, id) = base_ref(&root, base.as_deref(), &cancel).await?;
            (Some(name), Some(id))
        } else {
            (None, None)
        };
        let range = base_id
            .as_ref()
            .map_or_else(|| head.clone(), |base| format!("{base}..{head}"));
        let output = checked(
            git(
                &root,
                &args(&[
                    "log",
                    "--topo-order",
                    "--diff-merges=first-parent",
                    "--root",
                    "--numstat",
                    "-z",
                    "--find-renames",
                    "--no-color",
                    "--diff-algorithm=histogram",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--format=%x00COMMIT%x00%H%x00%an%x00%ar%x00%s%x00%B%x00",
                    &format!("--max-count={}", PAGE_SIZE + 1),
                    &format!("--skip={skip}"),
                    &range,
                    "--",
                ]),
                &cancel,
            )
            .await?,
        )?;
        let mut commits = parse_log(&output)?;
        let more = commits.len() > PAGE_SIZE;
        commits.truncate(PAGE_SIZE);
        Ok(Page {
            commits,
            base,
            base_id,
            head: Some(head),
            more,
        })
    })
    .await
    .map_err(|_| "git history timed out after 120 seconds".to_string())?
}

fn parse_log(output: &[u8]) -> Result<Vec<Commit>, String> {
    let mut fields = output.split(|byte| *byte == 0);
    let mut commits: Vec<Commit> = Vec::new();
    while let Some(field) = fields.next() {
        if field == b"COMMIT" {
            let mut next = || {
                fields
                    .next()
                    .map(|value| String::from_utf8_lossy(value).into_owned())
                    .ok_or_else(|| "invalid Git history record".to_string())
            };
            commits.push(Commit {
                id: next()?,
                author: next()?,
                age: next()?,
                subject: next()?,
                message: next()?,
                additions: 0,
                deletions: 0,
                binary: false,
            });
        } else if let Some(commit) = commits.last_mut() {
            let mut columns = field.splitn(3, |byte| *byte == b'\t');
            let add = columns.next().unwrap_or_default();
            let Some(del) = columns.next() else { continue };
            if columns.next() == Some(b"".as_slice()) {
                // A -z rename carries two additional NUL-delimited paths.
                fields.next();
                fields.next();
            }
            if add == b"-" || add == b"\n-" {
                commit.binary = true;
                continue;
            }
            if let (Ok(add), Ok(del)) = (
                String::from_utf8_lossy(add).trim().parse::<u64>(),
                String::from_utf8_lossy(del).parse::<u64>(),
            ) {
                commit.additions += add;
                commit.deletions += del;
            }
        }
    }
    Ok(commits)
}

/// A commit is compared to its first parent; a branch is compared to its merge base.
pub async fn comparison(
    root: PathBuf,
    revision: String,
    base: Option<String>,
    context: usize,
    cancel: CancellationToken,
) -> Result<DiffSnapshot, String> {
    tokio::time::timeout(Duration::from_secs(120), async {
        let revision = resolve(&root, &revision, &cancel).await?;
        let (before, label) = if let Some(base) = base {
            let base_id = resolve(&root, &base, &cancel).await?;
            let ancestor =
                checked(git(&root, &args(&["merge-base", &base_id, &revision]), &cancel).await?)?;
            (
                String::from_utf8_lossy(&ancestor).trim().to_string(),
                format!("{base}...{}", &revision[..8]),
            )
        } else {
            let parents = checked(
                git(
                    &root,
                    &args(&["rev-list", "--parents", "-n", "1", &revision, "--"]),
                    &cancel,
                )
                .await?,
            )?;
            let parents = String::from_utf8_lossy(&parents);
            let parent = match parents.split_whitespace().nth(1) {
                Some(parent) => parent.to_string(),
                None => empty_tree(&root, &cancel).await?,
            };
            (parent, format!("{} (first parent)", &revision[..8]))
        };
        let patch = checked(
            git(
                &root,
                &args(&[
                    "diff",
                    "--patch",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-relative",
                    "--src-prefix=a/",
                    "--dst-prefix=b/",
                    "--unified=2147483647",
                    "--diff-algorithm=histogram",
                    "--find-renames",
                    "--submodule=short",
                    "--ignore-submodules=none",
                    &before,
                    &revision,
                    "--",
                ]),
                &cancel,
            )
            .await?,
        )?;
        let worker_cancel = cancel.clone();
        let view = tokio::task::spawn_blocking(move || {
            let diff = Diff::parse_comparison(String::from_utf8_lossy(&patch).into_owned(), || {
                worker_cancel.is_cancelled()
            })
            .ok_or("cancelled")?;
            Ok::<_, String>(Arc::new(DiffView::new(Arc::new(diff), context)))
        });
        let view = tokio::select! {
            _ = cancel.cancelled() => return Err("cancelled".into()),
            view = view => view.map_err(|err| err.to_string())??,
        };
        Ok(DiffSnapshot {
            root,
            branch: label,
            view,
        })
    })
    .await
    .map_err(|_| "git comparison timed out after 120 seconds".to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_buffer::document::RowSource;

    fn run(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().into()
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["init", "-qb", "main"]);
        run(dir.path(), &["config", "user.name", "test"]);
        run(
            dir.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        dir
    }

    #[tokio::test]
    async fn branch_review_uses_merge_base_and_excludes_working_changes() {
        let dir = repo();
        let root = dir.path();
        std::fs::write(root.join("file"), "base\n").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "-qm", "root commit"]);
        let initial = run(root, &["rev-parse", "HEAD"]);
        run(root, &["checkout", "-qb", "feature"]);
        std::fs::write(root.join("file"), "feature\nextra\n").unwrap();
        run(root, &["commit", "-qam", "feature commit"]);
        let feature = run(root, &["rev-parse", "HEAD"]);
        run(root, &["checkout", "-q", "main"]);
        std::fs::write(root.join("main-only"), "upstream\n").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "-qm", "main commit"]);
        run(root, &["checkout", "-q", "feature"]);
        std::fs::write(root.join("file"), "uncommitted\n").unwrap();
        run(root, &["add", "."]);
        std::fs::write(root.join("untracked"), "untracked\n").unwrap();
        let index = std::fs::read(root.join(".git/index")).unwrap();
        run(root, &["tag", "main", "HEAD"]);
        let page = history(root.into(), None, true, None, 0, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(page.base.as_deref(), Some("main"));
        assert_eq!(page.commits.len(), 1);
        assert_eq!(page.commits[0].id, feature);
        let refreshed = history(
            root.into(),
            page.base.clone(),
            true,
            None,
            0,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(refreshed.base_id, page.base_id);
        assert_eq!(refreshed.commits.len(), 1);
        assert_eq!(
            (page.commits[0].additions, page.commits[0].deletions),
            (2, 1)
        );
        run(root, &["branch", "-f", "main", &feature]);
        let pinned = history(
            root.into(),
            page.base_id.clone(),
            true,
            page.head,
            0,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(pinned.commits.len(), 1);
        let snapshot = comparison(
            root.into(),
            feature,
            page.base_id,
            3,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(snapshot.view.diff.files.len(), 1);
        let file = &snapshot.view.diff.files[0];
        assert_eq!(file.path, "file");
        assert_eq!(file.section, smelt_transcript::diff::Section::Changes);
        assert_eq!((file.additions, file.deletions), (2, 1));
        assert!(update_index(
            root.into(),
            &snapshot.view.diff.files,
            IndexAction::Stage,
            CancellationToken::new()
        )
        .await
        .unwrap_err()
        .contains("read-only"));
        let tree = snapshot.view.tree();
        let rows = tree.rows(
            0..tree.snapshot().total_rows,
            &crate::theme::Theme::default(),
        );
        assert!(rows[0].text.contains("changes (1)"));
        assert!(!rows.iter().any(|row| row.text.contains("staged")));
        let root_commit = comparison(root.into(), initial, None, 3, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(root_commit.view.diff.files[0].additions, 1);
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), index);
        assert!(comparison(
            root.into(),
            "--all".into(),
            None,
            3,
            CancellationToken::new()
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn history_counts_renames_binary_merges_and_pages_from_pinned_head() {
        let dir = repo();
        let root = dir.path();
        std::fs::write(root.join("old"), "same\n").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "-qm", "initial"]);
        run(root, &["mv", "old", "COMMIT"]);
        std::fs::write(root.join("binary"), b"\0abc").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "-qm", "rename and binary"]);
        run(root, &["checkout", "-qb", "side"]);
        std::fs::write(root.join("side"), "side\n").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "-qm", "side change"]);
        run(root, &["checkout", "-q", "main"]);
        run(root, &["merge", "--no-ff", "-qm", "merge side", "side"]);
        let page = history(root.into(), None, false, None, 0, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(page.commits.len(), 4);
        assert_eq!(page.commits[0].subject, "merge side");
        assert_eq!(page.commits[0].additions, 1);
        let renamed = page
            .commits
            .iter()
            .find(|commit| commit.subject == "rename and binary")
            .unwrap();
        assert_eq!(
            (renamed.additions, renamed.deletions, renamed.binary),
            (0, 0, true)
        );
        for _ in 0..PAGE_SIZE - 1 {
            run(root, &["commit", "--allow-empty", "-qm", "empty"]);
        }
        let page = history(root.into(), None, false, None, 0, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(page.commits.len(), PAGE_SIZE);
        assert!(page.more);
        run(root, &["commit", "--allow-empty", "-qm", "new head"]);
        let next = history(
            root.into(),
            None,
            false,
            page.head,
            PAGE_SIZE,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(next.commits.len(), 3);
        assert!(!next.more);
        assert_eq!(next.commits.last().unwrap().subject, "initial");
    }

    #[tokio::test]
    async fn empty_history_and_missing_base_are_explicit() {
        let dir = repo();
        let page = history(
            dir.path().into(),
            None,
            false,
            None,
            0,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(page.commits.is_empty());
        run(dir.path(), &["commit", "--allow-empty", "-qm", "initial"]);
        run(dir.path(), &["branch", "-m", "other"]);
        assert!(history(
            dir.path().into(),
            None,
            true,
            None,
            0,
            CancellationToken::new()
        )
        .await
        .unwrap_err()
        .contains("choose a comparison base"));
        let page = history(
            dir.path().into(),
            Some("HEAD".into()),
            true,
            None,
            0,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(page.commits.is_empty());
    }
}
