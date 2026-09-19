//! What the sidebar asks git, and how git's answers are read. Every question runs git
//! itself, so the sidebar agrees with the command line on what is ignored, renamed or
//! changed; the reading of an answer is kept apart from the asking, so it is tested on
//! captured output.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// How many commits one `git log` reads; the next page is read as the cursor nears the end.
pub const LOG_PAGE: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    Modified,
    Added,
    Deleted,
    Renamed,
}

impl Change {
    pub fn letter(self) -> &'static str {
        match self {
            Change::Modified => "M",
            Change::Added => "A",
            Change::Deleted => "D",
            Change::Renamed => "R",
        }
    }

    /// What one `git status` entry says of a file, both columns read as one, for the row's
    /// colour and for what opening it means.
    fn from_status(index: u8, worktree: u8) -> Self {
        let either = |code: u8| index == code || worktree == code;
        if either(b'?') {
            Change::Added
        } else if either(b'D') {
            Change::Deleted
        } else if either(b'R') {
            Change::Renamed
        } else if either(b'A') || either(b'C') {
            Change::Added
        } else {
            Change::Modified
        }
    }

    /// What one column of a `git status` entry says: nothing when it is blank, and an
    /// untracked file is an addition git has not been told about.
    fn from_status_column(code: u8) -> Option<Self> {
        match code {
            b' ' => None,
            b'?' => Some(Change::Added),
            code => Some(Change::from_name_status(code)),
        }
    }

    /// What a `--name-status` letter says of a file in a commit.
    fn from_name_status(status: u8) -> Self {
        match status {
            b'A' | b'C' => Change::Added,
            b'D' => Change::Deleted,
            b'R' => Change::Renamed,
            _ => Change::Modified,
        }
    }
}

/// A file git reports as changed, in the working tree or in a commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: PathBuf,
    pub change: Change,
    /// Where a renamed file came from, relative to the repository's top: a diff asked with
    /// both paths reads as a rename, with one alone as a new file.
    pub from: Option<String>,
    /// What is staged, git's first column; nothing for a file in a commit.
    pub staged: Option<Change>,
    /// What is not staged yet, git's second column; nothing for a file in a commit.
    pub unstaged: Option<Change>,
}

impl ChangedFile {
    /// A file git does not know: nothing to unstage, and discarding it deletes it.
    pub fn is_untracked(&self) -> bool {
        self.staged.is_none() && self.unstaged == Some(Change::Added)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub hash: String,
    pub short: String,
    /// The committer's date, which a rebase renews, so ages read in the list's order.
    pub time: i64,
    /// The committer's date as the committer's clock read it: `2026-09-13 16:36`.
    pub date: String,
    pub author: String,
    pub subject: String,
    /// The file the commit was reached by (a file's history, a blame), relative to the
    /// repository's top and named as it was in that commit.
    pub file: Option<String>,
    /// Where that file came from when the commit renamed it.
    pub file_from: Option<String>,
}

/// Who last changed a line, and in which commit; no commit when the change is not
/// committed yet.
#[derive(Debug, PartialEq, Eq)]
pub struct Blame {
    pub author: String,
    pub commit: Option<Commit>,
}

/// A line to blame: the file, its line from 0, and the text the line numbers count. A
/// relative path is relative to the repository's top.
pub struct BlameRequest {
    pub path: PathBuf,
    pub line: usize,
    pub text: BlameText,
}

/// The text a blamed line is counted in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlameText {
    /// A buffer's, unsaved changes and all.
    Buffer(String),
    /// The file on disk.
    Disk,
    /// The file as a revision left it.
    Revision(String),
}

impl BlameText {
    /// The revision the text is read at, none for what is not committed.
    pub fn revision(&self) -> Option<&str> {
        match self {
            Self::Revision(revision) => Some(revision),
            Self::Buffer(_) | Self::Disk => None,
        }
    }
}

pub type Answer<T> = Result<T, String>;

/// What git's answer reads as outside a repository: a fact about the folder, not a failure.
pub const NOT_A_REPOSITORY: &str = "not a git repository";

/// Whether `root` is inside a git repository: it or a folder above holds `.git`, a
/// directory or, in a worktree or submodule, a file. Looked up on disk, not asked of git,
/// so it is cheap enough to ask whenever the tabs are drawn anew.
pub fn inside_repository(root: &Path) -> bool {
    root.ancestors().any(|dir| dir.join(".git").exists())
}

/// A pathspec naming one path from the repository's top, taken literally.
pub fn pathspec(top_relative: &str) -> String {
    format!(":(top,literal){top_relative}")
}

/// Where `root` sits inside its repository, as git spells it: `""` or `"a/b/"`.
pub fn prefix(root: &Path) -> Answer<String> {
    let prefix = run(root, &["rev-parse", "--show-prefix"])?;
    Ok(String::from_utf8_lossy(&prefix).trim_end().to_string())
}

/// What `git status` names below `root`, ignore rules included.
pub fn status(root: &Path) -> Answer<Vec<ChangedFile>> {
    let prefix = prefix(root)?;
    let status = run(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    parse_status(&status, &prefix, root)
}

/// One page of history, newest first, starting `skip` commits down from HEAD.
pub fn log(root: &Path, skip: usize) -> Answer<Vec<Commit>> {
    let skip = format!("--skip={skip}");
    let count = format!("--max-count={LOG_PAGE}");
    let log = run(root, &[LOG, "-z", "--abbrev=7", LOG_FORMAT, &skip, &count])?;
    parse_log(&log)
}

/// How many commits the filter of the history looks through, newest first.
pub const WHOLE_LOG_CAP: usize = 100_000;

/// The history from HEAD, up to `WHOLE_LOG_CAP` commits, for the filter to look through.
pub fn whole_log(root: &Path) -> Answer<Vec<Commit>> {
    let count = format!("--max-count={WHOLE_LOG_CAP}");
    let log = run(root, &[LOG, "-z", "--abbrev=7", LOG_FORMAT, &count])?;
    parse_log(&log)
}

/// Whether `commit` is one the filter `lowercase` names: its hash starts with it, or its
/// subject or author contains it, case aside.
pub fn commit_matches(commit: &Commit, lowercase: &str) -> bool {
    commit.hash.starts_with(lowercase)
        || commit.subject.to_lowercase().contains(lowercase)
        || commit.author.to_lowercase().contains(lowercase)
}

/// The whole history of one file below `root`, followed across renames, each commit
/// carrying the name the file had in it. Read whole: `--follow` miscounts `--skip`.
pub fn file_log(root: &Path, path: &Path) -> Answer<Vec<Commit>> {
    let path = path.to_string_lossy();
    let log = run(
        root,
        &[
            LOG,
            "-z",
            "--abbrev=7",
            LOG_FORMAT,
            "--follow",
            "--name-status",
            "--",
            &path,
        ],
    )?;
    parse_log(&log)
}

const LOG: &str = "log";
const LOG_FORMAT: &str = "--format=%H%x1f%h%x1f%ct%x1f%cz%x1f%an%x1f%s";

/// What one commit changed below `root` (a merge against its first parent), and where the
/// root sits inside the repository, which the diffs are then asked with.
pub fn commit_files(root: &Path, hash: &str) -> Answer<(String, Vec<ChangedFile>)> {
    let prefix = prefix(root)?;
    let listing = run(
        root,
        &[
            "show",
            "--format=",
            "--name-status",
            "-z",
            "-M",
            "--diff-merges=first-parent",
            "--no-color",
            hash,
        ],
    )?;
    let files = parse_name_status(&listing, &prefix, root)?;
    Ok((prefix, files))
}

/// Full commit information, independent of the paths selected for its patch.
/// How the two people of a commit are labelled over its message; the review buffer
/// knows the lines by these, to set the names apart.
pub const AUTHOR_LABEL: &str = "Autor: ";
pub const COMMITTER_LABEL: &str = "Committer: ";

pub fn commit_text(root: &Path, hash: &str) -> Answer<String> {
    let output = run(
        root,
        &[
            "show",
            "--no-patch",
            "--no-color",
            "--format=%an <%ae>  %ai%x00%cn <%ce>  %ci%x00%P%x00%B",
            hash,
            "--",
        ],
    )?;
    let output = String::from_utf8_lossy(&output);
    let fields: Vec<_> = output.splitn(4, '\0').collect();
    let [author, committer, parents, message] = fields.as_slice() else {
        return Err("git show: incomplete commit information".into());
    };
    // Who wrote it and who put it in, and nothing else over the message: the parents,
    // the branches and the tags around it were more than anyone read.
    let _ = parents;
    let mut text = format!("{AUTHOR_LABEL}{author}\n{COMMITTER_LABEL}{committer}\n\n");
    for line in message.split_terminator('\n') {
        text.push_str(line);
        text.push('\n');
    }
    let (files, added, removed) = commit_stats(root, hash)?;
    text.push_str(&format!(
        "\nArchivos: {files} · Líneas añadidas: +{added} · Líneas eliminadas: −{removed}\n"
    ));
    Ok(text)
}

/// Count the entire commit, using the same first-parent comparison as its patch.
/// Without `-z`, Git quotes tabs/newlines in paths and emits one row per file,
/// including renames. Binary files count as files but have no textual line counts.
fn commit_stats(root: &Path, hash: &str) -> Answer<(usize, usize, usize)> {
    let stats = run(
        root,
        &[
            "show",
            "--format=",
            "--numstat",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-relative",
            "-M",
            "--diff-merges=first-parent",
            hash,
            "--",
        ],
    )?;
    let (mut files, mut added, mut removed) = (0, 0, 0);
    for row in String::from_utf8_lossy(&stats).lines() {
        let mut fields = row.splitn(3, '\t');
        let (Some(insertions), Some(deletions), Some(_path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            return Err("git show: invalid numstat row".into());
        };
        let count = |value: &str| -> Answer<usize> {
            if value == "-" {
                Ok(0)
            } else {
                value
                    .parse()
                    .map_err(|_| "git show: invalid numstat count".into())
            }
        };
        files += 1;
        added += count(insertions)?;
        removed += count(deletions)?;
    }
    Ok((files, added, removed))
}

/// A canonical unified patch, independent of the user's prefix/context settings,
/// narrowed to `pathspecs`. Review presentation is built separately from this transport.
pub fn show(root: &Path, hash: &str, pathspecs: &[String], full_context: bool) -> Answer<String> {
    let context = if full_context {
        "--unified=2147483647"
    } else {
        "--unified=3"
    };
    let mut args = vec![
        "show",
        "--format=",
        "--no-textconv",
        "--no-relative",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        context,
        "--no-color",
        "--no-ext-diff",
        "-M",
        "--diff-merges=first-parent",
        hash,
        "--",
    ];
    args.extend(pathspecs.iter().map(String::as_str));
    let patch = run(root, &args)?;
    Ok(String::from_utf8_lossy(&patch).into_owned())
}

/// What `file` changed in the working tree against the last commit, staged or not, as the
/// same canonical patch `show` gives for a commit. A file git does not know yet reads as
/// added whole.
pub fn working_diff(root: &Path, file: &ChangedFile, full_context: bool) -> Answer<String> {
    let context = if full_context {
        "--unified=2147483647"
    } else {
        "--unified=3"
    };
    let common = [
        "--no-textconv",
        "--no-relative",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        context,
        "--no-color",
        "--no-ext-diff",
    ];
    if file.is_untracked() {
        // Against nothing, which git answers with 1 for "they differ".
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["diff", "--no-index"])
            .args(common)
            .arg("--")
            .arg("/dev/null")
            .arg(file.path.strip_prefix(root).unwrap_or(&file.path))
            .output()
            .map_err(|err| format!("git: {err}"))?;
        if output.status.code() != Some(1) && !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "git diff: {}",
                stderr.lines().next().unwrap_or("failed")
            ));
        }
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let path = format!(":(literal){}", file.path.display());
    let from = file.from.as_deref().map(pathspec);
    // Before the first commit there is nothing to compare with but the index.
    let base = if run(root, &["rev-parse", "--verify", "--quiet", "HEAD"]).is_ok() {
        "HEAD"
    } else {
        "--cached"
    };
    let mut args = vec!["diff", base];
    args.extend(common);
    args.extend(["-M", "--", path.as_str()]);
    if let Some(from) = &from {
        args.push(from);
    }
    let patch = run(root, &args)?;
    Ok(String::from_utf8_lossy(&patch).into_owned())
}

/// Blames one line as git would the file with that text in it: a line changed since the last
/// commit belongs to no commit, and so does every line of a file git does not know.
pub fn blame(root: &Path, request: &BlameRequest) -> Answer<Blame> {
    let range = format!("{0},{0}", request.line + 1);
    let path = if request.path.is_relative() {
        let top = run(root, &["rev-parse", "--show-toplevel"])?;
        PathBuf::from(String::from_utf8_lossy(&top).trim_end()).join(&request.path)
    } else {
        request.path.clone()
    };
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(root)
        .args(["blame", "--porcelain", "-L", &range]);
    match &request.text {
        BlameText::Buffer(_) => command.args(["--contents", "-"]),
        BlameText::Disk => &mut command,
        BlameText::Revision(revision) => command.arg(revision),
    };
    let mut child = command
        .arg("--")
        .arg(&path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("git: {err}"))?;
    // git reads the whole text before it answers, and the answer for one line is short, so
    // writing it all first cannot wait on a full pipe.
    let written = match (child.stdin.take(), &request.text) {
        (Some(mut stdin), BlameText::Buffer(contents)) => stdin.write_all(contents.as_bytes()),
        _ => Ok(()),
    };
    let output = child
        .wait_with_output()
        .map_err(|err| format!("git blame: {err}"))?;
    // A git that gave up before reading says why; the broken pipe it leaves behind does not.
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr.lines().next().unwrap_or("failed").to_string();
        if request.text == BlameText::Disk && reason.ends_with("in HEAD") {
            return Ok(Blame {
                author: "Not Committed Yet".to_string(),
                commit: None,
            });
        }
        return Err(format!("git blame: {reason}"));
    }
    written.map_err(|err| format!("git blame: {err}"))?;
    parse_blame(&String::from_utf8_lossy(&output.stdout))
}

/// Stages `file` whole, its deletion included.
pub fn stage(root: &Path, file: &ChangedFile) -> Answer<()> {
    let path = format!(":(literal){}", file.path.display());
    run(root, &["add", "-A", "--", &path]).map(|_| ())
}

/// Takes `file` out of the index; a staged rename goes with the path it came from, or
/// that one would stay staged as a deletion.
pub fn unstage(root: &Path, file: &ChangedFile) -> Answer<()> {
    let path = format!(":(literal){}", file.path.display());
    // Unlike `restore --staged`, reset also works before the first commit.
    let mut args = vec!["reset", "--", path.as_str()];
    let from = file.from.as_deref().map(pathspec);
    if let Some(from) = &from {
        args.push(from);
    }
    run(root, &args).map(|_| ())
}

/// One hunk of a file's patch: where it sits on each side, and its lines, header first,
/// ready to be applied under the file's own header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: usize,
    pub old_count: usize,
    pub new_start: usize,
    pub new_count: usize,
    pub text: String,
}

impl Hunk {
    /// Whether line `line` of one side, the old with `old`, is among the hunk's lines,
    /// its context included: a hunk that only removes has no new lines of its own, and
    /// is found by the context around them.
    fn holds(&self, old: bool, line: usize) -> bool {
        let (start, count) = if old {
            (self.old_start, self.old_count)
        } else {
            (self.new_start, self.new_count)
        };
        line >= start && line < start + count.max(1)
    }
}

/// The hunks of `file` in the patch git gives for it right now — against the index, or
/// with `staged` the index against the last commit — with the header they apply under.
/// A hunk acted on is always taken from a fresh patch: what is on screen was read
/// against the last commit, and may be older than the file.
pub fn file_hunks(root: &Path, file: &ChangedFile, staged: bool) -> Answer<(String, Vec<Hunk>)> {
    let path = format!(":(literal){}", file.path.display());
    let from = file.from.as_deref().map(pathspec);
    let mut args = vec!["diff"];
    if staged {
        args.push("--cached");
    }
    args.extend([
        "--no-textconv",
        "--no-relative",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--unified=3",
        "--no-color",
        "--no-ext-diff",
        "-M",
        "--",
        path.as_str(),
    ]);
    if let Some(from) = &from {
        args.push(from);
    }
    let patch = run(root, &args)?;
    split_hunks(&String::from_utf8_lossy(&patch))
}

/// Cuts one file's patch into its header and its hunks.
pub fn split_hunks(patch: &str) -> Answer<(String, Vec<Hunk>)> {
    let mut header = String::new();
    let mut hunks: Vec<Hunk> = Vec::new();
    for line in patch.split_inclusive('\n') {
        if let Some(range) = line.strip_prefix("@@ ") {
            let mut parts = range.split_whitespace();
            let before = parts.next().ok_or("git diff: missing old range")?;
            let after = parts.next().ok_or("git diff: missing new range")?;
            let (old_start, old_count) = hunk_range(before, '-')?;
            let (new_start, new_count) = hunk_range(after, '+')?;
            hunks.push(Hunk {
                old_start,
                old_count,
                new_start,
                new_count,
                text: line.to_string(),
            });
        } else if let Some(hunk) = hunks.last_mut() {
            hunk.text.push_str(line);
        } else {
            header.push_str(line);
        }
    }
    Ok((header, hunks))
}

fn hunk_range(value: &str, prefix: char) -> Answer<(usize, usize)> {
    let value = value
        .strip_prefix(prefix)
        .ok_or("git diff: invalid range prefix")?;
    let (start, count) = value.split_once(',').unwrap_or((value, "1"));
    let start = start.parse().map_err(|_| "git diff: invalid line number")?;
    let count = count.parse().map_err(|_| "git diff: invalid line count")?;
    Ok((start, count))
}

/// The hunk among `hunks` that holds line `line` of the old side with `old`, of the new
/// side otherwise.
pub fn hunk_holding(hunks: &[Hunk], old: bool, line: usize) -> Option<&Hunk> {
    hunks.iter().find(|hunk| hunk.holds(old, line))
}

/// What is done to one hunk of a file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HunkAct {
    /// Into the index.
    Stage,
    /// Out of the index; the working tree keeps it.
    Unstage,
    /// Out of the working tree; the index keeps what it has.
    Discard,
}

/// Applies `hunk` under `header` as `act` says, through `git apply`, which takes the
/// patch on its standard input.
pub fn apply_hunk(root: &Path, header: &str, hunk: &Hunk, act: HunkAct) -> Answer<()> {
    let mut args = vec!["apply", "--whitespace=nowarn"];
    match act {
        HunkAct::Stage => args.push("--cached"),
        HunkAct::Unstage => args.extend(["--cached", "-R"]),
        HunkAct::Discard => args.push("-R"),
    }
    args.push("-");
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("git: {err}"))?;
    let written = match child.stdin.take() {
        Some(mut stdin) => stdin
            .write_all(header.as_bytes())
            .and_then(|_| stdin.write_all(hunk.text.as_bytes())),
        None => Ok(()),
    };
    let output = child
        .wait_with_output()
        .map_err(|err| format!("git apply: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr.lines().next().unwrap_or("failed").to_string();
        return Err(format!("git apply: {reason}"));
    }
    written.map_err(|err| format!("git apply: {err}"))
}

/// Throws working changes away, preserving the index as the confirmation promises.
/// An untracked file is deleted; a tracked file goes back to its staged contents.
pub fn discard(root: &Path, file: &ChangedFile) -> Answer<()> {
    if file.is_untracked() {
        return std::fs::remove_file(&file.path)
            .map_err(|err| format!("{}: {err}", file.path.display()));
    }
    if file.unstaged.is_none() {
        return Err("No unstaged changes to discard; unstage the file first".to_string());
    }
    let path = format!(":(literal){}", file.path.display());
    run(root, &["restore", "--", &path]).map(|_| ())
}

fn run(dir: &Path, args: &[&str]) -> Answer<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|err| format!("git: {err}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("not a git repository") {
            return Err(NOT_A_REPOSITORY.to_string());
        }
        let reason = stderr.lines().next().unwrap_or("failed").to_string();
        return Err(format!("git {}: {reason}", args[0]));
    }
    Ok(output.stdout)
}

/// Reads `git status --porcelain=v1 -z`: paths are relative to the repository's top, and
/// only those under `prefix`, the root's place in it, are kept, as paths below `root`.
fn parse_status(status: &[u8], prefix: &str, root: &Path) -> Answer<Vec<ChangedFile>> {
    let mut files = Vec::new();
    let mut entries = status.split(|byte| *byte == 0);
    while let Some(entry) = entries.next() {
        if entry.is_empty() {
            continue;
        }
        if entry.len() < 4 {
            return Err(format!(
                "git status: unreadable entry {:?}",
                String::from_utf8_lossy(entry)
            ));
        }
        let change = Change::from_status(entry[0], entry[1]);
        let staged = if entry[0] == b'?' {
            None
        } else {
            Change::from_status_column(entry[0])
        };
        let unstaged = Change::from_status_column(entry[1]);
        // A rename or a copy carries the path it came from as the next entry.
        let renamed = matches!(entry[0], b'R' | b'C') || matches!(entry[1], b'R' | b'C');
        let from = if renamed {
            entries
                .next()
                .map(|from| String::from_utf8_lossy(from).into_owned())
        } else {
            None
        };
        let path = String::from_utf8_lossy(&entry[3..]);
        let Some(inside) = path.strip_prefix(prefix) else {
            continue;
        };
        files.push(ChangedFile {
            path: root.join(inside),
            change,
            from,
            staged,
            unstaged,
        });
    }
    Ok(files)
}

/// Reads a `git log -z` in `LOG_FORMAT`, with or without `--name-status`: with it, a
/// commit's record is followed by the followed file's letter, its source when renamed,
/// and its path.
fn parse_log(log: &[u8]) -> Answer<Vec<Commit>> {
    let mut commits: Vec<Commit> = Vec::new();
    // A --name-status line follows its commit's record after a newline.
    let mut records = log
        .split(|byte| *byte == 0)
        .map(|record| record.strip_prefix(b"\n").unwrap_or(record));
    while let Some(record) = records.next() {
        if record.is_empty() {
            continue;
        }
        if !record.contains(&0x1f) {
            let from = if matches!(record[0], b'R' | b'C') {
                records.next()
            } else {
                None
            };
            let (Some(commit), Some(path)) = (commits.last_mut(), records.next()) else {
                return Err(format!(
                    "git log: unreadable entry {:?}",
                    String::from_utf8_lossy(record)
                ));
            };
            commit.file = Some(String::from_utf8_lossy(path).into_owned());
            commit.file_from = from.map(|from| String::from_utf8_lossy(from).into_owned());
            continue;
        }
        let record = String::from_utf8_lossy(record);
        let mut fields = record.splitn(6, '\x1f');
        let (Some(hash), Some(short), Some(time), Some(zone), Some(author), Some(subject)) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            return Err(format!("git log: unreadable entry {record:?}"));
        };
        let Ok(time) = time.parse() else {
            return Err(format!("git log: unreadable date {time:?}"));
        };
        commits.push(Commit {
            hash: hash.to_string(),
            short: short.to_string(),
            time,
            date: format_date(time, zone),
            author: author.to_string(),
            subject: subject.to_string(),
            file: None,
            file_from: None,
        });
    }
    Ok(commits)
}

/// Reads a `--name-status -z` listing: a letter, the source when renamed or copied, and
/// the path; only paths under `prefix` are kept, as paths below `root`.
fn parse_name_status(listing: &[u8], prefix: &str, root: &Path) -> Answer<Vec<ChangedFile>> {
    let mut files = Vec::new();
    let mut entries = listing.split(|byte| *byte == 0);
    while let Some(status) = entries.next() {
        let Some(letter) = status.first().copied() else {
            continue;
        };
        let from = if matches!(letter, b'R' | b'C') {
            let Some(from) = entries.next() else {
                return Err("git show: a rename without its source".into());
            };
            Some(String::from_utf8_lossy(from).into_owned())
        } else {
            None
        };
        let Some(path) = entries.next() else {
            return Err(format!(
                "git show: {} names no file",
                String::from_utf8_lossy(status)
            ));
        };
        let path = String::from_utf8_lossy(path);
        let Some(inside) = path.strip_prefix(prefix) else {
            continue;
        };
        files.push(ChangedFile {
            path: root.join(inside),
            change: Change::from_name_status(letter),
            from,
            staged: None,
            unstaged: None,
        });
    }
    Ok(files)
}

/// Reads one line of `git blame --porcelain`: the hash, then headers up to the tab that
/// starts the line's text. An all-zero hash is a line no commit holds.
fn parse_blame(answer: &str) -> Answer<Blame> {
    let mut lines = answer.lines();
    let hash = lines
        .next()
        .and_then(|first| first.split(' ').next())
        .ok_or("git blame: no answer")?;
    let mut author = "";
    let mut time = "";
    let mut zone = "";
    let mut subject = "";
    let mut file = "";
    for line in lines.take_while(|line| !line.starts_with('\t')) {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "author" => author = value,
            "committer-time" => time = value,
            "committer-tz" => zone = value,
            "summary" => subject = value,
            "filename" => file = value,
            _ => {}
        }
    }
    let commit = if hash.bytes().all(|byte| byte == b'0') {
        None
    } else {
        let Ok(time) = time.parse() else {
            return Err(format!("git blame: unreadable date {time:?}"));
        };
        Some(Commit {
            hash: hash.to_string(),
            short: hash.chars().take(7).collect(),
            time,
            date: format_date(time, zone),
            author: author.to_string(),
            subject: subject.to_string(),
            file: Some(file.to_string()),
            file_from: None,
        })
    };
    Ok(Blame {
        author: author.to_string(),
        commit,
    })
}

/// A moment as `2026-09-13 16:36` on the clock of a zone written as git writes it,
/// `+0200`; a zone that cannot be read is taken as UTC.
fn format_date(time: i64, zone: &str) -> String {
    let offset = zone
        .get(1..5)
        .filter(|digits| digits.bytes().all(|byte| byte.is_ascii_digit()))
        .and_then(|digits| {
            let hours: i64 = digits[..2].parse().ok()?;
            let minutes: i64 = digits[2..].parse().ok()?;
            let offset = (hours * 60 + minutes) * 60;
            match zone.as_bytes()[0] {
                b'+' => Some(offset),
                b'-' => Some(-offset),
                _ => None,
            }
        })
        .unwrap_or(0);
    let local = time + offset;
    let (days, seconds) = (local.div_euclid(86_400), local.rem_euclid(86_400));
    // Days since 1970 to a civil date, after Howard Hinnant's `civil_from_days`.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        seconds / 3600,
        seconds % 3600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_actions_preserve_staged_changes_and_treat_names_literally() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "--quiet"]).unwrap();
        let path = root.join("[a].txt");
        std::fs::write(&path, "base\n").unwrap();
        let file = status(root).unwrap().remove(0);
        stage(root, &file).unwrap();
        // A new repository has no HEAD yet, but unstage must still work.
        let file = status(root).unwrap().remove(0);
        unstage(root, &file).unwrap();
        assert!(status(root).unwrap()[0].is_untracked());
        stage(root, &file).unwrap();
        run(
            root,
            &[
                "-c",
                "user.name=sid Test",
                "-c",
                "user.email=sid@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "initial",
            ],
        )
        .unwrap();

        std::fs::write(&path, "staged\n").unwrap();
        let file = status(root).unwrap().remove(0);
        stage(root, &file).unwrap();
        let file = status(root).unwrap().remove(0);
        assert!(discard(root, &file).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "staged\n");

        std::fs::write(&path, "unstaged\n").unwrap();
        let file = status(root).unwrap().remove(0);
        discard(root, &file).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "staged\n");
        let files = status(root).unwrap();
        assert_eq!(files[0].staged, Some(Change::Modified));
        assert_eq!(files[0].unstaged, None);
    }

    #[test]
    fn a_hunk_is_staged_unstaged_and_discarded_on_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "--quiet"]).unwrap();
        let path = root.join("a.txt");
        let base: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        std::fs::write(&path, &base).unwrap();
        run(root, &["add", "a.txt"]).unwrap();
        run(
            root,
            &[
                "-c",
                "user.name=sid Test",
                "-c",
                "user.email=sid@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "initial",
            ],
        )
        .unwrap();
        // Two changes far enough apart for two hunks: the top edited, the bottom removed.
        let edited = base
            .replace("line 2\n", "line two\n")
            .replace("line 19\n", "");
        std::fs::write(&path, &edited).unwrap();
        let file = status(root).unwrap().remove(0);

        let (header, hunks) = file_hunks(root, &file, false).unwrap();
        assert!(header.starts_with("diff --git a/a.txt b/a.txt\n"));
        assert_eq!(hunks.len(), 2);
        // The second hunk holds the removed line by its old number, and the lines around
        // it by their new ones.
        assert_eq!(hunk_holding(&hunks, true, 19), Some(&hunks[1]));
        assert_eq!(hunk_holding(&hunks, false, 18), Some(&hunks[1]));
        assert_eq!(hunk_holding(&hunks, false, 2), Some(&hunks[0]));
        assert_eq!(hunk_holding(&hunks, false, 10), None);

        // Staging the first leaves the second in the working tree only.
        apply_hunk(root, &header, &hunks[0], HunkAct::Stage).unwrap();
        let file = status(root).unwrap().remove(0);
        assert_eq!(file.staged, Some(Change::Modified));
        assert_eq!(file.unstaged, Some(Change::Modified));
        let (_, staged) = file_hunks(root, &file, true).unwrap();
        assert_eq!(staged.len(), 1);
        assert!(staged[0].text.contains("+line two\n"));
        let (_, unstaged) = file_hunks(root, &file, false).unwrap();
        assert_eq!(unstaged.len(), 1);
        assert!(unstaged[0].text.contains("-line 19\n"));

        // Unstaged again, the index is as the commit left it; discarded, so is the file
        // around the change kept.
        let (header, staged) = file_hunks(root, &file, true).unwrap();
        apply_hunk(root, &header, &staged[0], HunkAct::Unstage).unwrap();
        let file = status(root).unwrap().remove(0);
        assert_eq!(file.staged, None);
        let (header, hunks) = file_hunks(root, &file, false).unwrap();
        assert_eq!(hunks.len(), 2);
        apply_hunk(root, &header, &hunks[1], HunkAct::Discard).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            base.replace("line 2\n", "line two\n")
        );
    }

    #[test]
    fn a_patch_is_cut_into_its_header_and_hunks() {
        let patch = "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n@@ -10,2 +10,1 @@\n x\n-y\n";
        let (header, hunks) = split_hunks(patch).unwrap();
        assert_eq!(
            header,
            "diff --git a/f b/f\nindex 1..2 100644\n--- a/f\n+++ b/f\n"
        );
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].text, "@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n");
        assert_eq!((hunks[1].old_start, hunks[1].old_count), (10, 2));
        assert_eq!((hunks[1].new_start, hunks[1].new_count), (10, 1));
        assert!(split_hunks("").unwrap().1.is_empty());
    }

    fn root() -> PathBuf {
        PathBuf::from("/repo/sub")
    }

    #[test]
    fn status_keeps_what_is_below_the_root_and_reads_a_rename() {
        let status =
            b" M sub/a.txt\0?? sub/new.txt\0R  sub/moved.txt\0sub/old.txt\0 D other/gone.txt\0";
        let files = parse_status(status, "sub/", &root()).unwrap();
        assert_eq!(
            files,
            vec![
                ChangedFile {
                    path: root().join("a.txt"),
                    change: Change::Modified,
                    from: None,
                    staged: None,
                    unstaged: Some(Change::Modified),
                },
                ChangedFile {
                    path: root().join("new.txt"),
                    change: Change::Added,
                    from: None,
                    staged: None,
                    unstaged: Some(Change::Added),
                },
                ChangedFile {
                    path: root().join("moved.txt"),
                    change: Change::Renamed,
                    from: Some("sub/old.txt".into()),
                    staged: Some(Change::Renamed),
                    unstaged: None,
                },
            ]
        );
    }

    #[test]
    fn status_reads_both_columns() {
        let status = b"MM a.txt\0A  b.txt\0D  c.txt\0 D d.txt\0?? e.txt\0";
        let files = parse_status(status, "", &root()).unwrap();
        let columns: Vec<(Option<Change>, Option<Change>)> = files
            .iter()
            .map(|file| (file.staged, file.unstaged))
            .collect();
        assert_eq!(
            columns,
            vec![
                (Some(Change::Modified), Some(Change::Modified)),
                (Some(Change::Added), None),
                (Some(Change::Deleted), None),
                (None, Some(Change::Deleted)),
                (None, Some(Change::Added)),
            ]
        );
        assert!(files[4].is_untracked());
        assert!(!files[1].is_untracked());
    }

    #[test]
    fn status_refuses_an_entry_it_cannot_read() {
        assert!(parse_status(b"M\0", "", &root()).is_err());
    }

    #[test]
    fn log_reads_a_page_of_commits() {
        let log = b"abc123full\x1fabc123f\x1f1700000000\x1f+0100\x1fAda Lovelace\x1ffirst: subject\0def456full\x1fdef456f\x1f1699999999\x1f+0000\x1fAlan Turing\x1fsecond\0";
        let commits = parse_log(log).unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].short, "abc123f");
        assert_eq!(commits[0].time, 1700000000);
        assert_eq!(commits[0].date, "2023-11-14 23:13");
        assert_eq!(commits[0].author, "Ada Lovelace");
        assert_eq!(commits[0].subject, "first: subject");
        assert_eq!(commits[0].file, None);
        assert_eq!(commits[1].subject, "second");
    }

    #[test]
    fn a_filter_names_a_commit_by_hash_subject_or_author() {
        let log = b"45f740db8abc\x1f45f740d\x1f10\x1f+0000\x1fAda Lovelace\x1fFix the Parser\0";
        let commit = &parse_log(log).unwrap()[0];
        assert!(commit_matches(commit, "45f740db8"));
        assert!(commit_matches(commit, "parser"));
        assert!(commit_matches(commit, "lovelace"));
        assert!(!commit_matches(commit, "740db8"));
        assert!(!commit_matches(commit, "turing"));
    }

    #[test]
    fn a_file_log_names_the_file_as_each_commit_had_it() {
        let log = b"h1\x1fh1\x1f10\x1f+0000\x1fa\x1fmoved it\0\nR100\0old/name.rs\0new/name.rs\0h2\x1fh2\x1f9\x1f+0000\x1fa\x1fwrote it\0\nA\0old/name.rs\0";
        let commits = parse_log(log).unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].file.as_deref(), Some("new/name.rs"));
        assert_eq!(commits[0].file_from.as_deref(), Some("old/name.rs"));
        assert_eq!(commits[1].file.as_deref(), Some("old/name.rs"));
        assert_eq!(commits[1].file_from, None);
    }

    #[test]
    fn log_refuses_an_unreadable_date() {
        assert!(parse_log(b"h\x1fh\x1fyesterday\x1f+0000\x1fa\x1fs\0").is_err());
    }

    #[test]
    fn a_commit_listing_reads_letters_sources_and_paths() {
        let listing = b"M\0sub/a.rs\0R090\0sub/from.rs\0sub/to.rs\0A\0elsewhere.rs\0";
        let files = parse_name_status(listing, "sub/", &root()).unwrap();
        assert_eq!(
            files,
            vec![
                ChangedFile {
                    path: root().join("a.rs"),
                    change: Change::Modified,
                    from: None,
                    staged: None,
                    unstaged: None,
                },
                ChangedFile {
                    path: root().join("to.rs"),
                    change: Change::Renamed,
                    from: Some("sub/from.rs".into()),
                    staged: None,
                    unstaged: None,
                },
            ]
        );
    }

    #[test]
    fn blame_reads_the_commit_of_a_line() {
        let answer = "4da9363b7cb5cab71da15e39617e64a4a4127c5a 10 10 1\n\
            author Santiago Corredoira\n\
            author-mail <s@example.com>\n\
            committer-time 1786727071\n\
            committer-tz +0200\n\
            summary docs: the subject\n\
            filename CLAUDE.md\n\
            \tthe line itself\n";
        let blame = parse_blame(answer).unwrap();
        assert_eq!(blame.author, "Santiago Corredoira");
        let commit = blame.commit.unwrap();
        assert_eq!(commit.short, "4da9363");
        assert_eq!(commit.time, 1786727071);
        assert_eq!(commit.date, "2026-08-14 19:04");
        assert_eq!(commit.author, "Santiago Corredoira");
        assert_eq!(commit.subject, "docs: the subject");
        assert_eq!(commit.file.as_deref(), Some("CLAUDE.md"));
    }

    #[test]
    fn dates_read_on_the_committers_clock() {
        assert_eq!(format_date(0, "+0000"), "1970-01-01 00:00");
        assert_eq!(format_date(0, "-0130"), "1969-12-31 22:30");
        assert_eq!(format_date(951_782_400, "+0000"), "2000-02-29 00:00");
        assert_eq!(format_date(0, "zone"), "1970-01-01 00:00");
    }

    #[test]
    fn blame_of_an_uncommitted_line_has_no_commit() {
        let answer = "0000000000000000000000000000000000000000 3 3 1\n\
            author Not Committed Yet\n\
            committer-time 1786727071\n\
            summary Version of CLAUDE.md from standard input\n\
            filename CLAUDE.md\n\
            \tnew line\n";
        let blame = parse_blame(answer).unwrap();
        assert_eq!(blame.author, "Not Committed Yet");
        assert_eq!(blame.commit, None);
    }
}
