//! Git snapshots and explicit file-level index operations, off the UI thread.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use smelt_transcript::diff::{Diff, DiffView};

pub mod review;

pub struct DiffSnapshot {
    pub root: PathBuf,
    pub view: Arc<DiffView>,
    /// Branch state of a working-tree snapshot; comparisons have none.
    pub tracking: Option<Tracking>,
}

/// Where the checkout stands relative to its upstream branch.
#[derive(Debug, Default)]
pub struct Tracking {
    /// Checked-out branch, or `None` on a detached HEAD.
    pub branch: Option<String>,
    /// `HEAD` commit, or `None` in a repository without commits.
    pub head: Option<String>,
    /// Upstream of `branch`, such as `origin/main`, when one is configured
    /// and still exists.
    pub upstream: Option<String>,
    /// Commits on `HEAD` that the upstream lacks, newest first, one page at most.
    pub unpushed: Vec<review::Commit>,
    /// Commits on the upstream that `HEAD` lacks, newest first, one page at most.
    pub unpulled: Vec<review::Commit>,
    /// Full counts, which exceed the lists when they were truncated.
    pub ahead: usize,
    pub behind: usize,
    /// Multi-step operation in progress: `rebase`, `merge`, `cherry-pick`,
    /// `revert` or `bisect`.
    pub operation: Option<&'static str>,
}

impl Tracking {
    /// The non-empty commit lists a file tree shows after its file sections.
    /// A checkout without an upstream has none.
    fn commit_sections(&self) -> Vec<smelt_transcript::diff::tree::CommitSection> {
        use smelt_transcript::diff::tree::{CommitList, CommitRow, CommitSection};
        let Some(upstream) = &self.upstream else {
            return Vec::new();
        };
        [
            (CommitList::Unpushed, self.ahead, &self.unpushed),
            (CommitList::Unpulled, self.behind, &self.unpulled),
        ]
        .into_iter()
        .filter(|(_, total, _)| *total > 0)
        .map(|(list, total, commits)| CommitSection {
            list,
            upstream: upstream.clone(),
            total,
            commits: commits
                .iter()
                .map(|commit| CommitRow {
                    id: commit.id.clone(),
                    subject: commit.subject.clone(),
                })
                .collect(),
        })
        .collect()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct FileStatus {
    pub index: char,
    pub worktree: char,
}

pub(crate) type Status = HashMap<Vec<u8>, FileStatus>;

#[cfg(test)]
fn file_status(file: &smelt_transcript::diff::File, status: &Status) -> (char, char) {
    let mut columns = status
        .get(&file.raw_path)
        .map_or((' ', ' '), |s| (s.index, s.worktree));
    if file.raw_path != file.raw_old_path {
        if let Some(old) = status.get(&file.raw_old_path) {
            if old.index == 'D' && matches!(columns.0, ' ' | 'A') {
                columns.0 = 'R';
            }
            if old.worktree == 'D' {
                columns.1 = 'R';
            }
        }
    }
    columns
}

/// File states plus branch, upstream and ahead/behind counts, all from one
/// `git status`. Branch fields fill `tracking`; its commit lists stay empty.
/// Renames are not paired: each side of one is its own entry, an addition
/// and a deletion, however the diff pairs them.
async fn read_status(
    root: &Path,
    cancel: &CancellationToken,
) -> Result<(Status, Tracking), String> {
    let output = checked(
        git(
            root,
            &args(&[
                "status",
                "--porcelain=v2",
                "--branch",
                "--no-renames",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ]),
            cancel,
        )
        .await?,
    )?;
    parse_status(&output)
}

fn parse_status(output: &[u8]) -> Result<(Status, Tracking), String> {
    let invalid = || "invalid Git status record".to_string();
    let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    // `.` marks an unchanged side in porcelain v2.
    let column = |byte: u8| if byte == b'.' { ' ' } else { char::from(byte) };
    let mut status = Status::new();
    let mut tracking = Tracking::default();
    let mut upstream_exists = false;
    let fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    for field in fields {
        if let Some(header) = field.strip_prefix(b"# ") {
            let mut parts = header.splitn(2, |byte| *byte == b' ');
            let (key, value) = (parts.next().unwrap_or_default(), parts.next());
            let value = value.filter(|value| !value.starts_with(b"("));
            match key {
                b"branch.oid" => tracking.head = value.map(text),
                b"branch.head" => tracking.branch = value.map(text),
                b"branch.upstream" => tracking.upstream = value.map(text),
                // Only printed while the upstream exists.
                b"branch.ab" => {
                    upstream_exists = true;
                    let counts = text(value.unwrap_or_default());
                    // `+<ahead> -<behind>`
                    let mut counts = counts
                        .split(' ')
                        .map(|count| count.trim_start_matches(['+', '-']).parse().unwrap_or(0));
                    tracking.ahead = counts.next().unwrap_or(0);
                    tracking.behind = counts.next().unwrap_or(0);
                }
                _ => {}
            }
            continue;
        }
        // Ordinary and unmerged records name their path after a fixed
        // number of space-separated fields; untracked ones after one.
        let kind = field[0];
        let spaces = match kind {
            b'1' => 8,
            b'u' => 10,
            b'?' => 1,
            _ => continue,
        };
        let mut parts = field.splitn(spaces + 1, |byte| *byte == b' ');
        let path = parts.nth(spaces).ok_or_else(invalid)?.to_vec();
        if kind == b'?' {
            // A path deleted from the index can also be untracked on disk.
            status
                .entry(path)
                .and_modify(|file| file.worktree = '?')
                .or_insert(FileStatus {
                    index: ' ',
                    worktree: '?',
                });
            continue;
        }
        let xy = field.get(2..4).ok_or_else(invalid)?;
        status.insert(
            path,
            FileStatus {
                index: column(xy[0]),
                worktree: column(xy[1]),
            },
        );
    }
    // A configured upstream that was deleted counts as none, so a push sets
    // a new one.
    if !upstream_exists {
        tracking.upstream = None;
    }
    Ok((status, tracking))
}

#[derive(Clone, Copy)]
pub(crate) enum IndexAction {
    Stage,
    Unstage,
    Toggle,
}

/// Mutate only the selected files' index entries in one Git operation. A toggle
/// acts on their shared section, not another section containing the same paths.
pub(crate) async fn update_index(
    root: PathBuf,
    files: &[smelt_transcript::diff::File],
    action: IndexAction,
    cancel: CancellationToken,
) -> Result<(), String> {
    let Some(first) = files.first() else {
        return Ok(());
    };
    if first.section == smelt_transcript::diff::Section::Changes {
        return Err("historical comparisons are read-only".into());
    }
    if files.iter().any(|file| file.section != first.section) {
        return Err("index selection must belong to one section".into());
    }
    let (status, tracking) = read_status(&root, &cancel).await?;
    let stage = match action {
        IndexAction::Stage => true,
        IndexAction::Unstage => false,
        IndexAction::Toggle => first.section == smelt_transcript::diff::Section::Unstaged,
    };
    let mut paths = Vec::new();
    for file in files {
        if let Some(current) = status.get(&file.raw_path) {
            if if stage {
                current.worktree != ' '
            } else {
                current.index != ' '
            } {
                paths.push(file.raw_path.clone());
            }
        }
        if file.raw_old_path != file.raw_path {
            if let Some(old) = status.get(&file.raw_old_path) {
                if if stage {
                    old.worktree == 'D'
                } else {
                    old.index == 'D'
                } {
                    paths.push(file.raw_old_path.clone());
                }
            }
        }
    }
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return Ok(());
    }
    if !stage
        && status.iter().any(|(other, state)| {
            if state.index == ' ' || paths.binary_search(other).is_ok() {
                return false;
            }
            let selected_ancestor = other.iter().enumerate().any(|(end, byte)| {
                *byte == b'/'
                    && paths
                        .binary_search_by(|path| path.as_slice().cmp(&other[..end]))
                        .is_ok()
            });
            selected_ancestor
                || (state.index != 'D' && {
                    let mut prefix = other.clone();
                    prefix.push(b'/');
                    let descendant = paths.partition_point(|path| path < &prefix);
                    paths
                        .get(descendant)
                        .is_some_and(|path| path.starts_with(&prefix))
                })
        })
    {
        return Err(
            "conflicting staged paths: unstage the conflicting directory changes first".into(),
        );
    }
    let input = nul_list(&paths);
    let argv = if stage {
        // update-index consumes exact file names, including deletions. A literal
        // `git add` pathspec would also stage an unrelated replacement directory.
        args(&["update-index", "--add", "--remove", "-z", "--stdin"])
    } else {
        let base = match tracking.head {
            Some(head) => head,
            None => empty_tree(&root, &cancel).await?,
        };
        let mut argv = args(&["reset", "--quiet"]);
        argv.push(OsString::from(base));
        argv.extend(args(&["--pathspec-from-file=-", "--pathspec-file-nul"]));
        argv
    };
    checked(git_input(&root, &argv, Some(&input), None, &cancel).await?)?;
    Ok(())
}

/// Throw away the selected unstaged changes: tracked files return to their
/// staged content and new files, untracked or intent-to-add, are deleted.
/// Staged changes stay.
pub(crate) async fn discard(
    root: PathBuf,
    files: &[smelt_transcript::diff::File],
    cancel: CancellationToken,
) -> Result<(), String> {
    if files
        .iter()
        .any(|file| file.section != smelt_transcript::diff::Section::Unstaged)
    {
        return Err("only unstaged changes can be discarded".into());
    }
    let (status, _) = read_status(&root, &cancel).await?;
    let (mut tracked, mut intended, mut untracked) = (Vec::new(), Vec::new(), Vec::new());
    for file in files {
        let path = file.raw_path.clone();
        // A renamed file's old path is still in the index.
        if file.raw_old_path != path {
            tracked.push(file.raw_old_path.clone());
        }
        match status.get(&path).map(|state| state.worktree) {
            Some(' ') | None => {}
            Some('?') => untracked.push(path),
            // `git add -N` records an empty entry: restoring it would empty
            // the file, so drop the entry and delete it like an untracked one.
            Some('A') => {
                intended.push(path.clone());
                untracked.push(path);
            }
            Some(_) => tracked.push(path),
        }
    }
    if !tracked.is_empty() {
        let argv = args(&[
            "restore",
            "--worktree",
            "--pathspec-from-file=-",
            "--pathspec-file-nul",
        ]);
        checked(git_input(&root, &argv, Some(&nul_list(&tracked)), None, &cancel).await?)?;
    }
    if !intended.is_empty() {
        let argv = args(&["update-index", "--force-remove", "-z", "--stdin"]);
        checked(git_input(&root, &argv, Some(&nul_list(&intended)), None, &cancel).await?)?;
    }
    // Git holds no copy of these files, so only they are removed.
    for path in untracked {
        let path = root.join(path_bytes(path));
        tokio::fs::remove_file(&path)
            .await
            .map_err(|error| format!("remove {}: {error}", path.display()))?;
    }
    Ok(())
}

/// What a hunk change does to the changes it selects.
#[derive(Clone, Copy)]
pub(crate) enum HunkAction {
    Index(IndexAction),
    Discard,
}

/// Stage, unstage or discard the changes `selection` picks from `diff`.
/// Modified text files change through one `git apply`: whole, or only their
/// selected hunks. It must match the index or worktree, so a stale snapshot
/// fails and changes none of them. Other files, such as new, deleted,
/// renamed or binary ones, change only whole, as [`update_index`] and
/// [`discard`] change them, after the patch applies.
pub(crate) async fn change_hunks(
    root: PathBuf,
    diff: &smelt_transcript::diff::Diff,
    selection: &smelt_transcript::diff::Selection,
    action: HunkAction,
    cancel: CancellationToken,
) -> Result<(), String> {
    use smelt_transcript::diff::Section;
    let files = &diff.files;
    let mut chosen = selection.files();
    let Some(first) = chosen.next() else {
        return Ok(());
    };
    let section = files[first].section;
    if chosen.any(|file| files[file].section != section) {
        return Err("select changes of one section".into());
    }
    if section == Section::Changes {
        return Err("historical comparisons are read-only".into());
    }
    let patchable = |file: usize| !files[file].binary && files[file].status == "M";
    if let Some((file, _)) = selection.partial.iter().find(|(file, _)| !patchable(*file)) {
        return Err(format!(
            "{} changes only as a whole; select all of it",
            files[*file].path
        ));
    }
    let unstaged = section == Section::Unstaged;
    // Which way the patch applies: forward stages, backwards unstages or
    // discards, and only discarding touches the worktree.
    let (apply, reverse, cached) = match action {
        HunkAction::Index(IndexAction::Stage) => (unstaged, false, true),
        HunkAction::Index(IndexAction::Unstage) => (!unstaged, true, true),
        HunkAction::Index(IndexAction::Toggle) => (true, !unstaged, true),
        HunkAction::Discard if unstaged => (true, true, false),
        HunkAction::Discard => return Err("only unstaged changes can be discarded".into()),
    };
    if !apply {
        return Ok(());
    }
    let (patched, whole): (Vec<usize>, Vec<usize>) = selection
        .whole
        .iter()
        .copied()
        .partition(|&file| patchable(file));
    let mut parts = selection.partial.clone();
    parts.extend(
        patched
            .into_iter()
            .map(|file| (file, vec![diff.file_lines(file)])),
    );
    if !parts.is_empty() {
        let mut argv = args(&["apply", "--whitespace=nowarn"]);
        if cached {
            argv.push("--cached".into());
        }
        if reverse {
            argv.push("--reverse".into());
        }
        argv.push("-".into());
        let patch = diff.patch(&parts, reverse);
        checked(git_input(&root, &argv, Some(patch.as_bytes()), None, &cancel).await?)?;
    }
    let whole: Vec<_> = whole.into_iter().map(|file| files[file].clone()).collect();
    if whole.is_empty() {
        return Ok(());
    }
    match action {
        HunkAction::Index(action) => update_index(root, &whole, action, cancel).await,
        HunkAction::Discard => discard(root, &whole, cancel).await,
    }
}

/// Paths as NUL-terminated stdin for `-z --stdin` and `--pathspec-file-nul`.
fn nul_list(paths: &[Vec<u8>]) -> Vec<u8> {
    paths
        .iter()
        .flat_map(|path| path.iter().copied().chain([0]))
        .collect()
}

/// The empty tree, which an unborn branch and a root commit compare against.
async fn empty_tree(root: &Path, cancel: &CancellationToken) -> Result<String, String> {
    let id = checked(
        git(
            root,
            &args(&["hash-object", "-t", "tree", "--stdin"]),
            cancel,
        )
        .await?,
    )?;
    Ok(String::from_utf8_lossy(&id).trim().to_string())
}

async fn git(
    root: &Path,
    args: &[OsString],
    cancel: &CancellationToken,
) -> Result<std::process::Output, String> {
    git_input(root, args, None, None, cancel).await
}

async fn git_input(
    root: &Path,
    args: &[OsString],
    input: Option<&[u8]>,
    index: Option<&PrivateIndex>,
    cancel: &CancellationToken,
) -> Result<std::process::Output, String> {
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    let mut command = Command::new("git");
    command.current_dir(root).args([
        "--no-pager",
        "--literal-pathspecs",
        "-c",
        "core.quotePath=true",
        "-c",
        "diff.suppressBlankEmpty=false",
    ]);
    if let Some(index) = index {
        command
            .args(["-c", "core.splitIndex=false", "-c", "core.fsmonitor=false"])
            .env("GIT_INDEX_FILE", index.directory.path().join("index"))
            .env(
                "GIT_OBJECT_DIRECTORY",
                index.directory.path().join("objects"),
            )
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &index.alternates);
    }
    command
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = async {
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        let write = async {
            if let (Some(mut stdin), Some(input)) = (stdin, input) {
                stdin.write_all(input).await?;
            }
            Ok::<_, std::io::Error>(())
        };
        let (_, output) = tokio::try_join!(write, child.wait_with_output())?;
        Ok::<_, std::io::Error>(output)
    };
    tokio::select! {
        _ = cancel.cancelled() => Err("cancelled".into()),
        result = output => result.map_err(|error| format!("git: {error}")),
    }
}

fn operation(git_dir: &Path) -> Option<&'static str> {
    [
        ("rebase-merge", "rebase"),
        ("rebase-apply", "rebase"),
        ("MERGE_HEAD", "merge"),
        ("CHERRY_PICK_HEAD", "cherry-pick"),
        ("REVERT_HEAD", "revert"),
        ("BISECT_LOG", "bisect"),
    ]
    .into_iter()
    .find(|(marker, _)| git_dir.join(marker).exists())
    .map(|(_, name)| name)
}

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn checked(output: std::process::Output) -> Result<Vec<u8>, String> {
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn path_bytes(bytes: Vec<u8>) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes)
    }
    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(&bytes).as_ref())
    }
}

/// Intent-to-add entries let one Git diff include every untracked file, with
/// Git's own attributes, filters, encodings and binary handling. Both index and
/// object writes are isolated; the repository's index and objects are read-only.
struct PrivateIndex {
    directory: tempfile::TempDir,
    alternates: OsString,
}

impl PrivateIndex {
    async fn new(root: &Path, cancel: &CancellationToken) -> Result<Self, String> {
        async fn git_path(
            root: &Path,
            name: &str,
            cancel: &CancellationToken,
        ) -> Result<PathBuf, String> {
            let mut bytes = checked(
                git(
                    root,
                    &args(&["rev-parse", "--path-format=absolute", "--git-path", name]),
                    cancel,
                )
                .await?,
            )?;
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
            }
            Ok(PathBuf::from(path_bytes(bytes)))
        }
        let (index, objects) = tokio::try_join!(
            git_path(root, "index", cancel),
            git_path(root, "objects", cancel),
        )?;
        tokio::task::spawn_blocking(move || -> std::io::Result<Self> {
            use std::fmt::Write;
            let directory = tempfile::tempdir()?;
            std::fs::create_dir(directory.path().join("objects"))?;
            match std::fs::copy(index, directory.path().join("index")) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            // Git accepts C-quoted alternate paths. Octal quoting also handles
            // path-list separators, newlines and non-UTF-8 Unix directory names.
            let mut quoted = String::from("\"");
            for byte in objects.as_os_str().as_encoded_bytes() {
                write!(quoted, "\\{byte:03o}").unwrap();
            }
            quoted.push('"');
            let mut alternates = OsString::from(quoted);
            if let Some(existing) = std::env::var_os("GIT_ALTERNATE_OBJECT_DIRECTORIES") {
                alternates.push(if cfg!(windows) { ";" } else { ":" });
                alternates.push(existing);
            }
            Ok(Self {
                directory,
                alternates,
            })
        })
        .await
        .map_err(|error| format!("diff temporary index: {error}"))?
        .map_err(|error| format!("diff temporary index: {error}"))
    }
}

pub async fn working_diff(
    cwd: PathBuf,
    context: usize,
    cancel: CancellationToken,
) -> Result<DiffSnapshot, String> {
    tokio::time::timeout(
        Duration::from_secs(120),
        load_working_diff(cwd, context, cancel),
    )
    .await
    .map_err(|_| "git diff timed out after 120 seconds".to_string())?
}

async fn load_working_diff(
    cwd: PathBuf,
    context: usize,
    cancel: CancellationToken,
) -> Result<DiffSnapshot, String> {
    let mut dirs = checked(
        git(
            &cwd,
            &args(&["rev-parse", "--show-toplevel", "--absolute-git-dir"]),
            &cancel,
        )
        .await?,
    )?;
    if dirs.last() == Some(&b'\n') {
        dirs.pop();
    }
    let split = dirs
        .iter()
        .rposition(|byte| *byte == b'\n')
        .ok_or("git rev-parse: missing repository paths")?;
    let git_dir = PathBuf::from(path_bytes(dirs.split_off(split + 1)));
    dirs.pop();
    let root = PathBuf::from(path_bytes(dirs));
    let (status, mut tracking) = read_status(&root, &cancel).await?;
    tracking.operation = operation(&git_dir);
    let base = if tracking.head.is_some() {
        "HEAD".to_string()
    } else {
        empty_tree(&root, &cancel).await?
    };

    // Full context is indexed once, then folded without loading source blobs or
    // running Git again. Scrolling/expanding never shells out or reparses a patch.
    let options = [
        "--patch",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--no-relative",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--unified=2147483647",
        "--diff-algorithm=histogram",
        "--submodule=short",
        "--ignore-submodules=none",
    ];
    let mut diff_args = args(&["diff", "--ours"]);
    diff_args.extend(args(&options));
    diff_args.push("--".into());
    let mut staged_args = args(&["diff", "--cached"]);
    staged_args.extend(args(&options));
    staged_args.extend(args(&[&base, "--"]));
    let index = if status.values().any(|state| state.worktree == '?') {
        let index = PrivateIndex::new(&root, &cancel).await?;
        // One root pathspec avoids all-pairs matching between untracked paths
        // and directory entries. Intent-to-add honors ignore rules without
        // hashing tracked contents; all writes stay inside the private index.
        checked(
            git_input(
                &root,
                &args(&["add", "--intent-to-add", "--ignore-removal", "--", "."]),
                None,
                Some(&index),
                &cancel,
            )
            .await?,
        )?;
        // With every untracked file listed, only embedded repositories show
        // as directories. Their contents are not this repository's, and one
        // without a commit has nothing to diff, so drop them again.
        let embedded: Vec<Vec<u8>> = status
            .iter()
            .filter(|(path, state)| state.worktree == '?' && path.ends_with(b"/"))
            .map(|(path, _)| path[..path.len() - 1].to_vec())
            .collect();
        if !embedded.is_empty() {
            let argv = args(&["update-index", "--force-remove", "-z", "--stdin"]);
            checked(
                git_input(
                    &root,
                    &argv,
                    Some(&nul_list(&embedded)),
                    Some(&index),
                    &cancel,
                )
                .await?,
            )?;
        }
        Some(index)
    } else {
        None
    };
    let untracked = async {
        let Some(index) = &index else {
            return Ok(Vec::new());
        };
        let mut argv = args(&["diff", "--diff-filter=A", "--no-renames"]);
        argv.extend(args(&options));
        argv.push("--".into());
        checked(git_input(&root, &argv, None, Some(index), &cancel).await?)
    };
    // Tracked changes always use the real index. Adding an untracked directory
    // to a private index can displace its deleted tracked ancestor.
    // Commit lists exist only when the counts say there is something to list.
    let commits = |count: usize, range: &'static str| {
        let (root, cancel) = (&root, &cancel);
        async move {
            if count == 0 {
                return Ok(Vec::new());
            }
            Ok(review::log(root, range, 0, cancel).await?.0)
        }
    };
    let (unstaged, untracked, staged, unpushed, unpulled) = tokio::try_join!(
        git(&root, &diff_args, &cancel),
        untracked,
        git(&root, &staged_args, &cancel),
        commits(tracking.ahead, "@{upstream}..HEAD"),
        commits(tracking.behind, "HEAD..@{upstream}"),
    )?;
    tracking.unpushed = unpushed;
    tracking.unpulled = unpulled;
    let commits = tracking.commit_sections();
    let (unstaged, staged) = (checked(unstaged)?, checked(staged)?);
    let index_cancel = cancel.child_token();
    let _index_guard = index_cancel.clone().drop_guard();
    let index_status = status;
    let view = tokio::task::spawn_blocking(move || {
        let mut patch = String::from_utf8_lossy(&unstaged).into_owned();
        let untracked_start = patch.len();
        patch.push_str(&String::from_utf8_lossy(&untracked));
        let staged_start = patch.len();
        patch.push_str(&String::from_utf8_lossy(&staged));
        let mut diff = Diff::parse_sections(
            patch,
            staged_start,
            || index_cancel.is_cancelled(),
            |path, offset| {
                offset < untracked_start
                    || offset >= staged_start
                    || index_status
                        .get(path)
                        .is_some_and(|status| status.worktree == '?')
            },
        )
        .ok_or_else(|| "cancelled".to_string())?;
        let mut unmerged: std::collections::HashSet<_> = index_status
            .iter()
            .filter(|(_, state)| {
                state.index == 'U'
                    || state.worktree == 'U'
                    || matches!((state.index, state.worktree), ('A', 'A') | ('D', 'D'))
            })
            .map(|(path, _)| path.clone())
            .collect();
        for file in &mut diff.files {
            if file.section == smelt_transcript::diff::Section::Unstaged {
                if unmerged.remove(&file.raw_path) {
                    file.status = "U";
                } else if file.status == "A"
                    && index_status
                        .get(&file.raw_path)
                        .is_some_and(|status| status.worktree == '?')
                {
                    file.status = "?";
                }
            }
        }
        if !unmerged.is_empty() {
            for path in unmerged {
                diff.add_unmerged_file(path);
            }
            diff.sort_files(&|| index_cancel.is_cancelled())
                .ok_or("cancelled")?;
        }
        Ok::<_, String>(Arc::new(DiffView::with_commits(
            Arc::new(diff),
            context,
            &commits,
        )))
    });
    let view = tokio::select! {
        _ = cancel.cancelled() => return Err("cancelled".into()),
        view = view => view.map_err(|error| format!("diff index: {error}"))??,
    };
    Ok(DiffSnapshot {
        root,
        view,
        tracking: Some(tracking),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_buffer::document::RowSource;
    use smelt_transcript::diff::Section;

    fn run(root: &Path, argv: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(argv)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    async fn file_states(root: &Path) -> Status {
        read_status(root, &CancellationToken::new())
            .await
            .unwrap()
            .0
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

    async fn tracking(root: PathBuf) -> Tracking {
        working_diff(root, 3, CancellationToken::new())
            .await
            .unwrap()
            .tracking
            .unwrap()
    }

    #[tokio::test]
    async fn tracking_lists_unpushed_and_unpulled_commits_from_local_refs() {
        let dir = repo();
        let origin = dir.path().join("origin.git");
        let work = dir.path().join("work");
        let other = dir.path().join("other");
        run(
            dir.path(),
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                origin.to_str().unwrap(),
            ],
        );
        for clone in [&work, &other] {
            run(
                dir.path(),
                &[
                    "clone",
                    "-q",
                    origin.to_str().unwrap(),
                    clone.to_str().unwrap(),
                ],
            );
            run(clone, &["config", "user.name", "test"]);
            run(clone, &["config", "user.email", "test@example.invalid"]);
        }
        std::fs::write(work.join("file"), "base\n").unwrap();
        run(&work, &["add", "."]);
        run(&work, &["commit", "-qm", "base"]);
        run(&work, &["push", "-q", "-u", "origin", "main"]);
        run(&other, &["pull", "-q", "origin", "main"]);
        std::fs::write(other.join("theirs"), "a\nb\n").unwrap();
        run(&other, &["add", "."]);
        run(&other, &["commit", "-qm", "theirs"]);
        run(&other, &["push", "-q", "origin", "main"]);
        std::fs::write(work.join("file"), "ours\n").unwrap();
        run(&work, &["commit", "-qam", "ours"]);

        let stale = tracking(work.clone()).await;
        assert_eq!(stale.branch.as_deref(), Some("main"));
        assert_eq!(stale.upstream.as_deref(), Some("origin/main"));
        assert_eq!((stale.ahead, stale.behind), (1, 0), "no fetch runs");

        run(&work, &["fetch", "-q"]);
        let status = tracking(work.clone()).await;
        assert_eq!((status.ahead, status.behind), (1, 1));
        let subjects = |commits: &[review::Commit]| {
            commits
                .iter()
                .map(|c| c.subject.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(subjects(&status.unpushed), ["ours"]);
        assert_eq!(subjects(&status.unpulled), ["theirs"]);
        assert_eq!(status.operation, None);

        let merge = std::process::Command::new("git")
            .current_dir(&work)
            .args(["merge", "--no-commit", "--no-ff", "origin/main"])
            .output()
            .unwrap();
        assert!(
            merge.status.success(),
            "{}",
            String::from_utf8_lossy(&merge.stderr)
        );
        let merging = tracking(work.clone()).await;
        assert_eq!(merging.operation, Some("merge"));

        run(&other, &["checkout", "-qb", "gone"]);
        run(&other, &["push", "-q", "-u", "origin", "gone"]);
        run(&other, &["push", "-q", "origin", "--delete", "gone"]);
        run(&other, &["fetch", "-q", "--prune"]);
        assert_eq!(
            tracking(other).await.upstream,
            None,
            "a deleted upstream counts as none"
        );
    }

    #[tokio::test]
    async fn tracking_reports_detached_and_unborn_heads_without_upstream() {
        let dir = repo();
        let root = dir.path().to_path_buf();
        let unborn = tracking(root.clone()).await;
        assert_eq!(unborn.branch.as_deref(), Some("main"));
        assert_eq!((unborn.head, unborn.upstream), (None, None));

        std::fs::write(root.join("file"), "base\n").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-qm", "base"]);
        run(&root, &["checkout", "-q", "--detach"]);
        let detached = tracking(root).await;
        assert_eq!(detached.branch, None);
        assert!(detached.head.is_some());
        assert_eq!(detached.upstream, None);
    }

    fn repository_files(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        let mut files = std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.extend(repository_files(&path));
            } else {
                files.insert(path.clone(), std::fs::read(path).unwrap());
            }
        }
        files
    }

    #[tokio::test]
    async fn working_snapshot_includes_staged_unstaged_untracked_renames_and_binary() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "-q"]);
        run(root, &["config", "user.name", "test"]);
        run(root, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(root.join("tracked"), "base\n").unwrap();
        std::fs::write(root.join("rename me"), "rename body\n").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "-qm", "base"]);
        std::fs::write(root.join("tracked"), "staged\n").unwrap();
        run(root, &["add", "tracked"]);
        std::fs::write(root.join("tracked"), "unstaged\n").unwrap();
        run(root, &["mv", "rename me", "renamed file"]);
        std::fs::write(root.join("new\t界"), "untracked\n").unwrap();
        std::fs::create_dir(root.join("space b")).unwrap();
        std::fs::write(root.join("space b/binary"), [0u8, 1, 2]).unwrap();
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(snapshot.view.diff.files.len(), 5);
        assert!(snapshot
            .view
            .diff
            .files
            .iter()
            .any(|file| file.status == "R" && file.path == "renamed file"));
        assert!(snapshot
            .view
            .diff
            .files
            .iter()
            .any(|file| file.path == "new\t界"));
        assert!(
            snapshot
                .view
                .diff
                .files
                .iter()
                .any(|file| file.path == "space b/binary"),
            "{:?}",
            snapshot.view.diff.files
        );
        let rows = snapshot.view.rows(0..100, &crate::theme::Theme::default());
        assert!(rows.iter().any(|row| row.text == "+ unstaged"));
        assert!(rows.iter().any(|row| row.text == "+ staged"));
        assert!(rows.iter().any(|row| row.text == "- staged"));
        assert!(rows.iter().any(|row| row.text.contains("Binary files")));
    }

    #[tokio::test]
    async fn embedded_repositories_stay_out_of_untracked_files() {
        let dir = repo();
        let root = dir.path();
        std::fs::write(root.join("a"), "a\n").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "-qm", "base"]);
        std::fs::write(root.join("new"), "x\n").unwrap();
        for name in ["empty", "committed"] {
            std::fs::create_dir(root.join(name)).unwrap();
            run(&root.join(name), &["init", "-q"]);
        }
        run(
            &root.join("committed"),
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "nested",
            ],
        );
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        let paths: Vec<_> = snapshot.view.diff.files.iter().map(|f| &f.path).collect();
        assert_eq!(paths, ["new"]);
    }

    #[tokio::test]
    async fn discard_restores_staged_content_and_removes_new_files() {
        let dir = repo();
        let root = dir.path();
        for name in ["edited", "deleted", "kept"] {
            std::fs::write(root.join(name), "base\n").unwrap();
        }
        std::fs::write(root.join("moved"), "moved\n").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "-qm", "base"]);
        std::fs::write(root.join("edited"), "staged\n").unwrap();
        run(root, &["add", "edited"]);
        std::fs::write(root.join("edited"), "unstaged\n").unwrap();
        std::fs::remove_file(root.join("deleted")).unwrap();
        std::fs::write(root.join("kept"), "keep me\n").unwrap();
        std::fs::write(root.join("new\t界"), "untracked\n").unwrap();
        std::fs::write(root.join("intended"), "intent to add\n").unwrap();
        run(root, &["add", "-N", "intended"]);
        std::fs::rename(root.join("moved"), root.join("renamed")).unwrap();
        run(root, &["add", "-N", "renamed"]);
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        let files = &snapshot.view.diff.files;
        let unstaged = |path: &str| {
            files
                .iter()
                .find(|file| file.path == path && file.section == Section::Unstaged)
                .unwrap()
                .clone()
        };
        let selected = ["edited", "deleted", "new\t界", "intended", "renamed"].map(unstaged);
        discard(root.to_owned(), &selected, CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(std::fs::read(root.join("edited")).unwrap(), b"staged\n");
        assert_eq!(std::fs::read(root.join("deleted")).unwrap(), b"base\n");
        assert!(!root.join("new\t界").exists());
        assert!(!root.join("intended").exists());
        assert!(!root.join("renamed").exists());
        assert_eq!(std::fs::read(root.join("moved")).unwrap(), b"moved\n");
        assert!(
            !file_states(root).await.contains_key(b"intended".as_slice()),
            "no index entry left"
        );
        assert_eq!(std::fs::read(root.join("kept")).unwrap(), b"keep me\n");
        let staged = files
            .iter()
            .find(|file| file.section == Section::Staged)
            .unwrap()
            .clone();
        assert!(
            discard(root.to_owned(), &[staged], CancellationToken::new())
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deletion_mode_change_symlink_and_empty_untracked_are_visible() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "-q"]);
        run(root, &["config", "user.name", "test"]);
        run(root, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(root.join("deleted"), "old\n").unwrap();
        std::fs::write(root.join("executable"), "script\n").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "-qm", "base"]);
        std::fs::remove_file(root.join("deleted")).unwrap();
        std::fs::set_permissions(
            root.join("executable"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink("missing", root.join("link")).unwrap();
        std::fs::write(root.join("empty"), "").unwrap();
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        let files = &snapshot.view.diff.files;
        assert_eq!(files.len(), 4, "{files:?}");
        assert!(files
            .iter()
            .any(|file| file.path == "deleted" && file.status == "D"));
        assert!(files
            .iter()
            .any(|file| file.path == "executable" && file.status == "T"));
        assert!(files
            .iter()
            .any(|file| file.path == "link" && file.status == "?"));
        assert!(files
            .iter()
            .any(|file| file.path == "empty" && file.status == "?"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn untracked_rows_match_git_attributes_filters_encodings_modes_and_newlines() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "-q"]);
        run(root, &["config", "core.autocrlf", "true"]);
        run(root, &["config", "filter.canonical.clean", "tr a-z A-Z"]);
        run(root, &["config", "diff.forced.binary", "true"]);
        std::fs::write(
            root.join(".gitattributes"),
            "*.force -diff\n*.text diff\n*.clean filter=canonical\n*.utf16 text working-tree-encoding=UTF-16LE\n*.driver diff=forced\n",
        ).unwrap();
        async fn compare(root: &Path, snapshot: &DiffSnapshot, path: &str) {
            let cancel = CancellationToken::new();
            let expected = git(
                root,
                &args(&[
                    "diff",
                    "--no-index",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--src-prefix=a/",
                    "--dst-prefix=b/",
                    "--",
                    "/dev/null",
                    path,
                ]),
                &cancel,
            )
            .await
            .unwrap();
            assert!(expected.status.success() || expected.status.code() == Some(1));
            let mut expected = Diff::parse(String::from_utf8_lossy(&expected.stdout).into_owned());
            expected.files[0].status = "?";
            let expected = DiffView::new(Arc::new(expected), 3);
            let file = snapshot
                .view
                .diff
                .files
                .iter()
                .position(|file| file.path == path)
                .unwrap();
            let start = snapshot.view.file_row(file).unwrap();
            let end = snapshot
                .view
                .file_row(file + 1)
                .map_or(u64::MAX, |row| row - 2);
            let rows = |rows: Vec<smelt_buffer::document::DocumentRow>| {
                rows.into_iter()
                    .map(|row| (row.text, row.decoration.source_line))
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                rows(
                    snapshot
                        .view
                        .rows(start..end, &crate::theme::Theme::default())
                ),
                rows(expected.rows(0..u64::MAX, &crate::theme::Theme::default())),
                "{path}"
            );
        }
        let cases = [
            ("normal.rs", &b"let value = 42;\n"[..]),
            ("crlf.rs", &b"first\r\nsecond\r\n"[..]),
            ("no-newline", &b"first\nlast"[..]),
            ("empty", &b""[..]),
            ("empty.force", &b""[..]),
            ("binary", &b"a\0b"[..]),
            ("binary.text", &b"a\0b"[..]),
            ("ascii.force", &b"text\n"[..]),
            ("quoted \"\\\t界.rs", &b"source\n"[..]),
            ("invalid-utf8", &[0xff, b'\n'][..]),
            ("executable", &b"#!/bin/sh\n"[..]),
            ("filter.clean", &b"normalized\n"[..]),
            ("encoding.utf16", &b"a\0b\0\n\0"[..]),
            ("binary.driver", &b"source\n"[..]),
            (":(glob)*.[rs]", &b"literal path\n"[..]),
        ];
        for (path, data) in cases {
            std::fs::write(root.join(path), data).unwrap();
            if path == "executable" {
                std::fs::set_permissions(root.join(path), std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
        }
        symlink("missing\n界", root.join("link")).unwrap();
        let before = repository_files(&root.join(".git"));
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(before, repository_files(&root.join(".git")));
        assert_eq!(snapshot.view.diff.files.len(), cases.len() + 2);
        for (path, _) in cases {
            compare(root, &snapshot, path).await;
        }
        compare(root, &snapshot, "link").await;
        let rows = snapshot.view.rows(0..200, &crate::theme::Theme::default());
        assert!(rows.iter().any(|row| row.text == "+ NORMALIZED"));
        assert!(rows.iter().any(|row| row.text == "+ ab"));
    }

    #[tokio::test]
    async fn snapshot_preserves_linked_worktree_split_index_and_keeps_index_only_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir
            .path()
            .join(if cfg!(unix) { "repo: \"界" } else { "repo" });
        std::fs::create_dir(&root).unwrap();
        run(&root, &["init", "-q"]);
        run(&root, &["config", "user.name", "test"]);
        run(&root, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(root.join("tracked"), "base\n").unwrap();
        std::fs::write(root.join("recreated"), "base\n").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored*\n").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-qm", "base"]);
        run(&root, &["worktree", "add", "-qb", "linked", "../linked"]);
        let worktree = dir.path().join("linked");
        std::fs::write(worktree.join("tracked"), "staged\n").unwrap();
        run(&worktree, &["add", "tracked"]);
        std::fs::write(worktree.join("tracked"), "base\n").unwrap();
        run(&worktree, &["rm", "-q", "recreated"]);
        std::fs::write(worktree.join("recreated"), "replacement\n").unwrap();
        std::fs::write(worktree.join("ignored-tracked"), "included\n").unwrap();
        run(&worktree, &["add", "-f", "ignored-tracked"]);
        std::fs::write(worktree.join("ignored-untracked"), "excluded\n").unwrap();
        std::fs::write(worktree.join("new"), "new\n").unwrap();
        run(&worktree, &["config", "core.splitIndex", "true"]);
        run(&worktree, &["update-index", "--split-index"]);
        let before = repository_files(&root.join(".git"));
        let snapshot = working_diff(worktree.clone(), 3, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(before, repository_files(&root.join(".git")));
        assert_eq!(snapshot.root, worktree);
        assert_eq!(snapshot.tracking.unwrap().branch.as_deref(), Some("linked"));
        let files = &snapshot.view.diff.files;
        assert_eq!(files.len(), 6, "{files:?}");
        for path in ["tracked", "recreated"] {
            let entries: Vec<_> = files.iter().filter(|f| f.path == path).collect();
            assert_eq!(entries.len(), 2);
            assert_eq!(
                entries[0].section,
                smelt_transcript::diff::Section::Unstaged
            );
            assert_eq!(entries[1].section, smelt_transcript::diff::Section::Staged);
        }
        assert!(files
            .iter()
            .any(|f| f.path == "recreated" && f.status == "?"));
        assert!(files
            .iter()
            .any(|f| f.path == "recreated" && f.status == "D"));
        let rows = snapshot.view.rows(0..100, &crate::theme::Theme::default());
        assert!(rows.iter().any(|row| row.text == "+ included"));
        assert!(rows.iter().any(|row| row.text == "+ replacement"));
        assert!(rows.iter().any(|row| row.text == "+ staged"));
        assert!(rows.iter().any(|row| row.text == "- staged"));
        assert!(rows.iter().any(|row| row.text == "+ base"));
        assert!(rows.iter().any(|row| row.text == "- base"));
    }

    #[tokio::test]
    async fn index_operations_handle_unborn_renames_deletions_and_cancelled_requests() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "-q"]);
        run(root, &["config", "user.name", "test"]);
        run(root, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(root.join("new"), "new\n").unwrap();
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        let file = snapshot.view.diff.files[0].clone();
        update_index(
            root.to_owned(),
            std::slice::from_ref(&file),
            IndexAction::Stage,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = file_states(root).await;
        assert_eq!(file_status(&file, &status), ('A', ' '));
        update_index(
            root.to_owned(),
            std::slice::from_ref(&file),
            IndexAction::Unstage,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = file_states(root).await;
        assert_eq!(file_status(&file, &status), (' ', '?'));
        assert_eq!(std::fs::read_to_string(root.join("new")).unwrap(), "new\n");
        run(root, &["add", "."]);
        run(root, &["commit", "-qm", "base"]);
        run(root, &["mv", "new", "renamed"]);
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        let file = snapshot.view.diff.files[0].clone();
        assert_eq!(file.status, "R");
        update_index(
            root.to_owned(),
            std::slice::from_ref(&file),
            IndexAction::Unstage,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = file_states(root).await;
        assert_eq!(status[b"new".as_slice()].worktree, 'D');
        assert_eq!(status[b"renamed".as_slice()].worktree, '?');
        update_index(
            root.to_owned(),
            std::slice::from_ref(&file),
            IndexAction::Stage,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = file_states(root).await;
        assert_eq!(file_status(&file, &status), ('R', ' '));
        assert!(!root.join("new").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("renamed")).unwrap(),
            "new\n"
        );
        run(root, &["commit", "-qm", "rename"]);
        std::fs::remove_file(root.join("renamed")).unwrap();
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        let file = snapshot.view.diff.files[0].clone();
        let before = repository_files(&root.join(".git"));
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(update_index(
            root.to_owned(),
            std::slice::from_ref(&file),
            IndexAction::Stage,
            cancel
        )
        .await
        .is_err());
        assert_eq!(before, repository_files(&root.join(".git")));
        update_index(
            root.to_owned(),
            std::slice::from_ref(&file),
            IndexAction::Toggle,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = file_states(root).await;
        assert_eq!(file_status(&file, &status), ('D', ' '));
        let staged = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        update_index(
            root.to_owned(),
            &staged.view.diff.files[..1],
            IndexAction::Toggle,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = file_states(root).await;
        assert_eq!(file_status(&file, &status), (' ', 'D'));
        assert!(!root.join("renamed").exists());
        std::fs::write(root.join(".git/index.lock"), "held").unwrap();
        assert!(update_index(
            root.to_owned(),
            &[file],
            IndexAction::Stage,
            CancellationToken::new()
        )
        .await
        .unwrap_err()
        .contains("index.lock"));
    }

    #[tokio::test]
    async fn index_operations_preserve_replacements_and_resolve_net_zero_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "-q"]);
        run(root, &["config", "user.name", "test"]);
        run(root, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(root.join("old"), "original\n").unwrap();
        std::fs::write(root.join("zero"), "base\n").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "-qm", "base"]);
        run(root, &["mv", "old", "renamed"]);
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        let renamed = snapshot
            .view
            .diff
            .files
            .iter()
            .find(|f| f.path == "renamed")
            .unwrap()
            .clone();
        std::fs::write(root.join("old"), "replacement\n").unwrap();
        run(root, &["add", "old"]);
        let old_entry = checked(
            git(root, &args(&["show", ":old"]), &CancellationToken::new())
                .await
                .unwrap(),
        )
        .unwrap();
        update_index(
            root.to_owned(),
            &[renamed],
            IndexAction::Unstage,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            checked(
                git(root, &args(&["show", ":old"]), &CancellationToken::new())
                    .await
                    .unwrap()
            )
            .unwrap(),
            old_entry
        );
        assert_eq!(
            std::fs::read_to_string(root.join("renamed")).unwrap(),
            "original\n"
        );
        for action in [IndexAction::Stage, IndexAction::Unstage] {
            std::fs::write(root.join("zero"), "staged\n").unwrap();
            run(root, &["add", "zero"]);
            std::fs::write(root.join("zero"), "base\n").unwrap();
            let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
                .await
                .unwrap();
            let file = snapshot
                .view
                .diff
                .files
                .iter()
                .find(|f| f.path == "zero")
                .unwrap()
                .clone();
            assert_eq!(
                snapshot
                    .view
                    .diff
                    .files
                    .iter()
                    .filter(|f| f.path == "zero")
                    .count(),
                2
            );
            update_index(root.to_owned(), &[file], action, CancellationToken::new())
                .await
                .unwrap();
            let status = file_states(root).await;
            assert!(!status.contains_key(b"zero".as_slice()));
            assert_eq!(
                std::fs::read_to_string(root.join("zero")).unwrap(),
                "base\n"
            );
        }
        run(root, &["rm", "-q", "zero"]);
        std::fs::write(root.join("zero"), "replacement\n").unwrap();
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        let file = snapshot
            .view
            .diff
            .files
            .iter()
            .find(|f| f.path == "zero")
            .unwrap()
            .clone();
        assert_eq!(file_status(&file, &file_states(root).await), ('D', '?'));
        update_index(
            root.to_owned(),
            std::slice::from_ref(&file),
            IndexAction::Toggle,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = file_states(root).await;
        assert_eq!(file_status(&file, &status), ('M', ' '));
        update_index(
            root.to_owned(),
            &[file],
            IndexAction::Unstage,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("zero")).unwrap(),
            "replacement\n"
        );
        std::fs::remove_file(root.join("zero")).unwrap();
        std::fs::create_dir(root.join("zero")).unwrap();
        std::fs::write(root.join("zero/child"), "unrelated\n").unwrap();
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        let file = snapshot
            .view
            .diff
            .files
            .iter()
            .find(|f| f.path == "zero")
            .unwrap()
            .clone();
        update_index(
            root.to_owned(),
            &[file],
            IndexAction::Stage,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = file_states(root).await;
        assert_eq!(
            status[b"zero/child".as_slice()].index,
            ' ',
            "staging a deleted file staged its replacement directory"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("zero/child")).unwrap(),
            "unrelated\n"
        );
        run(root, &["add", "zero/child"]);
        let before = std::fs::read(root.join(".git/index")).unwrap();
        let file = snapshot
            .view
            .diff
            .files
            .iter()
            .find(|f| f.path == "zero")
            .unwrap()
            .clone();
        assert!(
            update_index(
                root.to_owned(),
                &[file],
                IndexAction::Unstage,
                CancellationToken::new()
            )
            .await
            .is_err(),
            "unstaging a file discarded staged descendants"
        );
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), before);
    }

    #[tokio::test]
    async fn batch_unstage_checks_file_directory_conflicts_not_name_prefixes() {
        for (old_path, new_path) in [("src/a", "src/a/child"), ("src/a/child", "src/a")] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            run(root, &["init", "-q"]);
            run(root, &["config", "user.name", "test"]);
            run(root, &["config", "user.email", "test@example.invalid"]);
            std::fs::create_dir_all(root.join(old_path).parent().unwrap()).unwrap();
            std::fs::write(root.join(old_path), "old file\n").unwrap();
            std::fs::write(root.join("src/ab"), "base\n").unwrap();
            run(root, &["add", "."]);
            run(root, &["commit", "-qm", "base"]);
            run(root, &["rm", "-q", old_path]);
            std::fs::create_dir_all(root.join(new_path).parent().unwrap()).unwrap();
            std::fs::write(root.join(new_path), "replacement\n").unwrap();
            std::fs::write(root.join("src/ab"), "changed\n").unwrap();
            run(root, &["add", "."]);
            let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
                .await
                .unwrap();
            let files = &snapshot.view.diff.files;
            assert_eq!(files.len(), 3);
            let removed = files.iter().find(|file| file.path == old_path).unwrap();
            let similar = files.iter().find(|file| file.path == "src/ab").unwrap();
            let before = std::fs::read(root.join(".git/index")).unwrap();
            let error = update_index(
                root.to_owned(),
                &[removed.clone(), similar.clone()],
                IndexAction::Unstage,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
            assert!(
                error.contains("conflicting staged paths"),
                "{old_path}: {error}"
            );
            assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), before);

            update_index(
                root.to_owned(),
                std::slice::from_ref(similar),
                IndexAction::Unstage,
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let status = file_states(root).await;
            assert_eq!(status[b"src/ab".as_slice()].index, ' ');
            assert_eq!(
                status.values().filter(|state| state.index != ' ').count(),
                2
            );

            update_index(
                root.to_owned(),
                files,
                IndexAction::Unstage,
                CancellationToken::new(),
            )
            .await
            .unwrap();
            run(root, &["diff", "--cached", "--exit-code"]);
            assert_eq!(
                std::fs::read_to_string(root.join(new_path)).unwrap(),
                "replacement\n"
            );
            assert_eq!(
                std::fs::read_to_string(root.join("src/ab")).unwrap(),
                "changed\n"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn staging_uses_literal_raw_paths_not_lossy_display_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "-q"]);
        let paths = [
            b"bad\xff.rs".to_vec(),
            b"bad\xfe.rs".to_vec(),
            b":(glob)*.rs".to_vec(),
            b"tab\tline\n.rs".to_vec(),
        ];
        for path in &paths {
            std::fs::write(root.join(path_bytes(path.clone())), b"source\n").unwrap();
        }
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        for path in &paths {
            let file = snapshot
                .view
                .diff
                .files
                .iter()
                .find(|file| file.raw_path == *path)
                .unwrap()
                .clone();
            update_index(
                root.to_owned(),
                std::slice::from_ref(&file),
                IndexAction::Stage,
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let status = file_states(root).await;
            assert_eq!(
                status.values().filter(|state| state.index != ' ').count(),
                1
            );
            assert_eq!(status[path].index, 'A');
            update_index(
                root.to_owned(),
                &[file],
                IndexAction::Unstage,
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let status = file_states(root).await;
            assert!(status.values().all(|state| state.index == ' '));
        }
        update_index(
            root.to_owned(),
            &snapshot.view.diff.files,
            IndexAction::Toggle,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let staged = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(staged.view.diff.files.len(), paths.len());
        assert!(staged
            .view
            .diff
            .files
            .iter()
            .all(|file| file.section == smelt_transcript::diff::Section::Staged));
        update_index(
            root.to_owned(),
            &staged.view.diff.files,
            IndexAction::Toggle,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = file_states(root).await;
        assert!(status.values().all(|state| state.index == ' '));
    }

    #[tokio::test]
    async fn unborn_clean_non_repository_and_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            working_diff(dir.path().to_owned(), 3, CancellationToken::new())
                .await
                .is_err()
        );
        run(dir.path(), &["init", "-q"]);
        let snapshot = working_diff(dir.path().to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        assert!(snapshot.view.diff.files.is_empty());
        std::fs::write(dir.path().join("new"), "new\n").unwrap();
        let snapshot = working_diff(dir.path().to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(snapshot.view.diff.files[0].status, "?");
        assert_eq!(
            snapshot.view.diff.files[0].section,
            smelt_transcript::diff::Section::Unstaged
        );
        run(dir.path(), &["add", "."]);
        let snapshot = working_diff(dir.path().to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(snapshot.view.diff.files[0].status, "A");
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(working_diff(dir.path().to_owned(), 3, cancel)
            .await
            .is_err());
    }
}
