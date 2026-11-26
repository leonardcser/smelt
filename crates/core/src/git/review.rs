//! Read-only revision comparisons and bounded commit pages.

use super::*;

#[derive(Debug)]
pub struct Commit {
    pub id: String,
    pub author: String,
    pub age: String,
    pub subject: String,
    pub message: String,
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
        let (commits, more) = log(&root, &range, skip, &cancel).await?;
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

/// One page of commits in `range`, newest first, and whether more follow.
pub(crate) async fn log(
    root: &Path,
    range: &str,
    skip: usize,
    cancel: &CancellationToken,
) -> Result<(Vec<Commit>, bool), String> {
    let output = checked(
        git(
            root,
            &args(&[
                "log",
                "--topo-order",
                "-z",
                "--no-color",
                "--format=%H%x00%an%x00%ar%x00%s%x00%B",
                &format!("--max-count={}", PAGE_SIZE + 1),
                &format!("--skip={skip}"),
                range,
                "--",
            ]),
            cancel,
        )
        .await?,
    )?;
    let mut commits = parse_log(&output)?;
    let more = commits.len() > PAGE_SIZE;
    commits.truncate(PAGE_SIZE);
    Ok((commits, more))
}

/// Line totals of one commit against its first parent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    pub additions: u64,
    pub deletions: u64,
    pub binary: bool,
}

/// Line totals of each commit in `ids`, against its first parent. Unlike a
/// commit list, this diffs every commit, so callers load it after the list.
pub async fn stats(
    root: PathBuf,
    ids: Vec<String>,
    cancel: CancellationToken,
) -> Result<HashMap<String, Stats>, String> {
    // Only object IDs are read from stdin, never options or revision syntax.
    if let Some(id) = ids
        .iter()
        .find(|id| id.len() < 4 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(format!("not a commit ID: {id}"));
    }
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let input: String = ids.iter().map(|id| format!("{id}\n")).collect();
    let argv = args(&[
        "log",
        "--no-walk=unsorted",
        "--stdin",
        "--diff-merges=first-parent",
        "--root",
        "--numstat",
        "--find-renames",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--format=%x01%H",
        "--",
    ]);
    let output = tokio::time::timeout(
        Duration::from_secs(120),
        git_input(&root, &argv, Some(input.as_bytes()), None, &cancel),
    )
    .await
    .map_err(|_| "git stats timed out after 120 seconds".to_string())??;
    Ok(parse_stats(&checked(output)?))
}

/// `\x01<id>` heads each commit; its `<added>\t<deleted>\t<path>` lines
/// follow, with `-` counts for a binary file.
fn parse_stats(output: &[u8]) -> HashMap<String, Stats> {
    let mut stats: Vec<(String, Stats)> = Vec::new();
    for line in String::from_utf8_lossy(output).lines() {
        if let Some(id) = line.strip_prefix('\x01') {
            stats.push((id.to_string(), Stats::default()));
            continue;
        }
        let (Some((_, commit)), Some((added, rest))) = (stats.last_mut(), line.split_once('\t'))
        else {
            continue;
        };
        let deleted = rest.split('\t').next().unwrap_or_default();
        match (added.parse::<u64>(), deleted.parse::<u64>()) {
            (Ok(added), Ok(deleted)) => {
                commit.additions += added;
                commit.deletions += deleted;
            }
            _ => commit.binary = true,
        }
    }
    stats.into_iter().collect()
}

/// Records are NUL-terminated under `-z`; their fields are NUL-separated.
fn parse_log(output: &[u8]) -> Result<Vec<Commit>, String> {
    let mut fields = output
        .strip_suffix(b"\0")
        .unwrap_or(output)
        .split(|byte| *byte == 0)
        .map(|field| String::from_utf8_lossy(field).into_owned());
    let mut commits = Vec::new();
    while let Some(id) = fields.next().filter(|id| !id.is_empty()) {
        let mut next = || fields.next().ok_or("invalid Git history record");
        commits.push(Commit {
            id,
            author: next()?,
            age: next()?,
            subject: next()?,
            message: next()?,
        });
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
        let before = if let Some(base) = base {
            let base_id = resolve(&root, &base, &cancel).await?;
            let ancestor =
                checked(git(&root, &args(&["merge-base", &base_id, &revision]), &cancel).await?)?;
            String::from_utf8_lossy(&ancestor).trim().to_string()
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
            match parents.split_whitespace().nth(1) {
                Some(parent) => parent.to_string(),
                None => empty_tree(&root, &cancel).await?,
            }
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
            view,
            tracking: None,
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

    async fn stats_rejects(root: &Path, id: &str) -> bool {
        stats(root.into(), vec![id.into()], CancellationToken::new())
            .await
            .is_err()
    }

    #[tokio::test]
    async fn history_pages_and_stats_cover_merges_renames_and_binaries() {
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
        assert_eq!(page.commits[1].message, "side change\n");
        let ids: Vec<_> = page
            .commits
            .iter()
            .map(|commit| commit.id.clone())
            .collect();
        let stats = stats(root.into(), ids.clone(), CancellationToken::new())
            .await
            .unwrap();
        let totals = |index: usize| {
            let stats = &stats[&ids[index]];
            (stats.additions, stats.deletions, stats.binary)
        };
        assert_eq!(totals(0), (1, 0, false), "a merge counts its first parent");
        assert_eq!(totals(2), (0, 0, true), "a pure rename adds no lines");
        assert_eq!(totals(3), (1, 0, false), "a root commit counts every line");
        for id in ["--output=x", "HEAD~1"] {
            assert!(stats_rejects(root, id).await, "{id}");
        }
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
