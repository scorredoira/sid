//! The Commits tab: the history, the whole repository's or one file's, and inside a commit
//! the files it touched, the diff of what the cursor is on shown in the editor.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use helix_core::unicode::width::UnicodeWidthStr;
use helix_view::editor::CommitFiles;
use helix_view::graphics::{Modifier, Style};
use helix_view::{Editor, Theme};
use tui::buffer::Buffer as Surface;

use super::diff_view::{DiffSource, DiffTarget};
use super::entries::{self, CommitRow, Folds, Row, RowPaint};
use super::git::{self, ChangedFile, Commit, LOG_PAGE};
use super::list::List;
use super::tab::{Activation, Message, Outcome, TabContext, TabView};
use super::{TabKind, REFRESH};

/// How long the cursor rests on a commit before its diff is asked for: a wheel or a held
/// arrow passes over many, and only the one it stops on is wanted.
const PREVIEW_DELAY: Duration = Duration::from_millis(150);

/// The most columns an author takes in a wide list: a longer name is cut.
const AUTHOR_MAX_WIDTH: usize = 20;

/// The columns the subject must still have for a list to show each commit's hash, date and
/// author before it; a narrower list shows the subject and its age alone.
const WIDE_SUBJECT_WIDTH: usize = 20;

pub struct CommitsTab {
    root: PathBuf,
    rows: Vec<Row>,
    list: List,
    history_rows: Vec<Row>,
    history_list: List,
    files_focused: bool,
    files_visible: bool,
    /// How the files were laid out the last time the rows were built.
    layout: CommitFiles,
    showing: Showing,
    /// Counts the times `showing` changed, so a page asked for the previous history is
    /// dropped when it lands.
    epoch: u32,
    /// The history read so far, newest first, or why it could not be read.
    log: Option<git::Answer<Vec<Commit>>>,
    /// Whether the last page came back short, so there is nothing older to read.
    complete: bool,
    /// The `skip` of the page being asked for.
    asking: Option<usize>,
    /// Whether the next read of the top page is already on its way.
    armed: bool,
    /// Whether F5 came while a page was being read, to read the top again when it lands.
    again: bool,
    /// Counts the cursor's moves, so the preview waited for is the last move's alone.
    moves: u32,
    opened: Option<OpenCommit>,
    /// The hash whose files are being asked for.
    opening: Option<String>,
    /// Whether that open was asked for — F9, Enter, a double click — so its files take the
    /// keys when they land; following the cursor over the history never moves them.
    opening_focus: bool,
    /// What the history is narrowed to while the filter box is open.
    filter: Option<String>,
    /// The whole history, read when the box opened, for the filter to look through; until
    /// it lands the filter looks through the pages read so far. A file's history is whole.
    whole: Option<Vec<Commit>>,
    /// The commits the filter keeps, which the history rows then list.
    filtered: Option<Vec<Commit>>,
    /// Whether the diff follows the cursor over the history: from the first move or click
    /// in it, so arriving at the tab does not take the editor's view away.
    follow: bool,
}

/// Whose history the tab lists.
#[derive(Clone, PartialEq, Eq)]
enum Showing {
    Repository,
    /// One file's, followed across renames.
    File(PathBuf),
}

/// A commit opened into its files, over the history it was chosen from.
struct OpenCommit {
    commit: Commit,
    /// Where the root sits inside the repository, as git spells it: `""` or `"a/b/"`.
    prefix: String,
    files: Vec<ChangedFile>,
    folds: Folds,
    /// Where the history stood, to put it back on the way out.
    list_cursor: usize,
    list_scroll: usize,
}

/// A page of history as it landed: for which history, from where, and what git said.
struct Page {
    epoch: u32,
    skip: usize,
    answer: git::Answer<Vec<Commit>>,
}

impl CommitsTab {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            rows: Vec::new(),
            list: List::default(),
            history_rows: Vec::new(),
            history_list: List::default(),
            files_focused: false,
            files_visible: false,
            layout: CommitFiles::Tree,
            showing: Showing::Repository,
            epoch: 0,
            log: None,
            complete: false,
            asking: None,
            armed: false,
            again: false,
            moves: 0,
            opened: None,
            opening: None,
            opening_focus: false,
            filter: None,
            whole: None,
            filtered: None,
            follow: false,
        }
    }

    /// The files are laid out again when the setting that says how has been changed
    /// since, from the settings screen or the palette; asked at render.
    pub fn follow_layout(&mut self, editor: &mut Editor) {
        if self.opened.is_some() && self.layout != editor.config().sidebar.commit_files {
            self.rebuild(editor);
        }
    }

    pub fn has_files(&self) -> bool {
        self.files_visible && self.opened.is_some()
    }

    pub fn files_visible(&self) -> bool {
        self.files_visible
    }

    /// F9: the files of the commit the history cursor is on, with the keys in them, or
    /// the pane put away.
    pub fn toggle_files(&mut self, cx: &mut TabContext) {
        self.set_files_visible(cx, !self.files_visible, true);
    }

    /// Shows the files of the commit the history cursor is on, or puts the pane away.
    /// With `focus` the keys go into the files; without it they stay in the history.
    fn set_files_visible(&mut self, cx: &mut TabContext, visible: bool, focus: bool) {
        self.files_visible = visible;
        self.files_focused = false;
        self.follow = true;
        if self.files_visible {
            let commit = self.listed().get(self.history_list.cursor).cloned();
            if let Some(commit) = commit {
                self.open_commit(commit, focus);
            } else {
                self.focus_files(focus);
            }
        }
        self.preview(cx);
    }

    pub fn files_focused(&self) -> bool {
        self.files_focused
    }

    pub fn filter(&self) -> Option<&str> {
        self.filter.as_deref()
    }

    /// Opens the filter box, or narrows the history to the commits `text` names while it
    /// is open, the cursor on the first; `None` closes it and the whole history comes back,
    /// the cursor on the commit it was on when the pages read so far hold it.
    pub fn set_filter(&mut self, editor: &mut Editor, text: Option<String>) {
        let under_cursor = self
            .listed()
            .get(self.history_list.cursor)
            .map(|commit| commit.hash.clone());
        if text.is_some() && self.filter.is_none() {
            self.read_whole();
        }
        let closing = text.is_none();
        if closing {
            self.whole = None;
        }
        self.filter = text;
        self.rebuild(editor);
        let found = under_cursor
            .filter(|_| closing)
            .and_then(|hash| self.listed().iter().position(|commit| commit.hash == hash));
        match found {
            Some(index) => {
                self.history_list.select(index);
                self.history_list.center();
            }
            None => self.history_list.home(),
        }
        self.ask_next_page_if_near_end();
        self.preview_when_rested();
    }

    /// Reads the whole history for the filter box, off the main thread; a file's history
    /// is read whole already.
    fn read_whole(&mut self) {
        if self.showing != Showing::Repository {
            return;
        }
        let root = self.root.clone();
        let epoch = self.epoch;
        super::background(
            move || git::whole_log(&root),
            move |sidebar, editor, answer| {
                let commits = &mut sidebar.commits;
                if commits.epoch != epoch || commits.filter.is_none() {
                    return;
                }
                match answer {
                    Ok(whole) => {
                        if whole.len() >= git::WHOLE_LOG_CAP {
                            editor.set_status(format!(
                                "the filter looks through the last {} commits only",
                                git::WHOLE_LOG_CAP
                            ));
                        }
                        commits.whole = Some(whole);
                    }
                    Err(err) => editor.set_error(err),
                }
                commits.rebuild(editor);
                commits.preview_when_rested();
            },
        );
    }

    /// The commits the history rows list: the ones the filter keeps while it is open.
    fn listed(&self) -> &[Commit] {
        match (&self.filtered, &self.log) {
            (Some(filtered), _) => filtered,
            (None, Some(Ok(commits))) => commits,
            _ => &[],
        }
    }

    pub fn focus_files(&mut self, files: bool) {
        self.files_focused = files && self.has_files();
    }

    pub fn panes(&self) -> [(&[Row], &List, bool); 2] {
        [
            (&self.history_rows, &self.history_list, !self.files_focused),
            (&self.rows, &self.list, self.files_focused),
        ]
    }

    pub fn set_pages(&mut self, history: usize, files: usize) {
        self.history_list.set_page(history);
        self.list.set_page(files);
    }

    /// Lists the history of one file in place of the whole one.
    pub fn show_history(&mut self, cx: &mut TabContext, path: PathBuf) {
        self.set_showing(Showing::File(path));
        self.rebuild(cx.editor);
        self.ask_page(0);
    }

    /// Opens `commit` into its files, shown under the history with the keys in them, as
    /// Enter on the commit would: for a commit reached from elsewhere, a blamed line.
    pub fn open_commit_with_files(&mut self, commit: Commit) {
        self.files_visible = true;
        self.follow = true;
        self.open_commit(commit, true);
    }

    /// Opens `commit` into its files, the cursor on the file the commit was reached by when
    /// it carries one. The files are asked of git; the commit opens when they land.
    pub fn open_commit(&mut self, commit: Commit, focus: bool) {
        let hash = commit.hash.clone();
        self.opening = Some(hash.clone());
        self.opening_focus = focus;
        let root = self.root.clone();
        super::background(
            move || git::commit_files(&root, &hash),
            move |sidebar, editor, answer| {
                let mut cx = TabContext {
                    editor,
                    diff: &mut sidebar.diff,
                };
                sidebar.commits.files_landed(&mut cx, commit, answer);
            },
        );
    }

    fn set_showing(&mut self, showing: Showing) {
        self.showing = showing;
        self.epoch = self.epoch.wrapping_add(1);
        self.log = None;
        self.complete = false;
        self.asking = None;
        self.opened = None;
        self.opening = None;
        self.filter = None;
        self.whole = None;
        self.files_focused = false;
        self.follow = false;
        self.history_list.home();
    }

    /// Asks git for the page of history starting `skip` commits down; one at a time.
    fn ask_page(&mut self, skip: usize) {
        if self.asking.is_some() {
            return;
        }
        self.asking = Some(skip);
        let root = self.root.clone();
        let epoch = self.epoch;
        let file = match &self.showing {
            Showing::Repository => None,
            Showing::File(path) => path.strip_prefix(&self.root).ok().map(Path::to_path_buf),
        };
        super::background(
            move || {
                let answer = match &file {
                    Some(file) => git::file_log(&root, file),
                    None => git::log(&root, skip),
                };
                Page {
                    epoch,
                    skip,
                    answer,
                }
            },
            |sidebar, editor, page| {
                let shown = sidebar.showing(TabKind::Commits);
                sidebar.commits.page_landed(shown, editor, page);
            },
        );
    }

    fn page_landed(&mut self, shown: bool, editor: &mut Editor, page: Page) {
        if page.epoch != self.epoch {
            return;
        }
        self.asking = None;
        if self.take_page(page) {
            self.rebuild(editor);
        }
        if self.again {
            self.again = false;
            self.ask_page(0);
        }
        // For as long as the tab is on screen, the top page is read again after the last
        // answer, so a commit or a rebase made elsewhere shows up. A file's history is read
        // whole, too much to read again every few seconds: R asks.
        let follows_head = self.showing == Showing::Repository;
        if shown && follows_head && !self.armed {
            self.armed = true;
            super::later(REFRESH, |sidebar, _editor| {
                sidebar.commits.armed = false;
                let repository = sidebar.commits.showing == Showing::Repository;
                if sidebar.showing(TabKind::Commits) && repository {
                    sidebar.commits.ask_page(0);
                }
            });
        }
        self.ask_next_page_if_near_end();
    }

    /// Folds a page into the history. The top page replaces the list when the history moved
    /// under it, keeping the cursor on its commit when that one is still there; a later
    /// page extends it. Returns whether the list changed.
    fn take_page(&mut self, page: Page) -> bool {
        let commits = match page.answer {
            Ok(commits) => commits,
            Err(err) => {
                self.log = Some(Err(err));
                return true;
            }
        };
        let complete = self.showing != Showing::Repository || commits.len() < LOG_PAGE;
        if page.skip > 0 {
            let Some(Ok(held)) = &mut self.log else {
                return false;
            };
            // A page of a list that was read again from the top meanwhile.
            if page.skip != held.len() {
                return false;
            }
            held.extend(commits);
            self.complete = complete;
            return true;
        }
        let held = match &self.log {
            Some(Ok(held)) => Some(held),
            _ => None,
        };
        let unchanged = held.is_some_and(|held| {
            held.first().map(|commit| &commit.hash) == commits.first().map(|commit| &commit.hash)
        });
        if unchanged {
            return false;
        }
        let under_cursor = held
            .and_then(|held| held.get(self.history_list.cursor))
            .map(|commit| commit.hash.clone());
        let found =
            under_cursor.and_then(|hash| commits.iter().position(|commit| commit.hash == hash));
        // While the filter is open the cursor stands in the commits it keeps.
        if let Some(index) = found.filter(|_| self.filter.is_none()) {
            self.history_list.select(index);
        }
        self.log = Some(Ok(commits));
        self.complete = complete;
        true
    }

    fn ask_next_page_if_near_end(&mut self) {
        let Some(Ok(commits)) = &self.log else {
            return;
        };
        if self.filter.is_some() {
            return;
        }
        if !self.complete && self.history_list.near_end() {
            self.ask_page(commits.len());
        }
    }

    fn files_landed(
        &mut self,
        cx: &mut TabContext,
        commit: Commit,
        answer: git::Answer<(String, Vec<ChangedFile>)>,
    ) {
        if self.opening.as_deref() != Some(commit.hash.as_str()) {
            return;
        }
        self.opening = None;
        let (prefix, files) = match answer {
            Ok(answer) => answer,
            Err(err) => {
                cx.editor.set_error(err);
                return;
            }
        };
        let target = commit
            .file
            .as_deref()
            .and_then(|file| file.strip_prefix(prefix.as_str()))
            .map(|inside| self.root.join(inside));
        let (list_cursor, list_scroll) = match &self.opened {
            Some(opened) => (opened.list_cursor, opened.list_scroll),
            None => (self.history_list.cursor, self.history_list.scroll),
        };
        self.opened = Some(OpenCommit {
            commit,
            prefix,
            files,
            folds: Folds::opened(),
            list_cursor,
            list_scroll,
        });
        self.files_focused = self.opening_focus && self.files_visible;
        self.follow = true;
        self.list.home();
        self.rebuild(cx.editor);
        if let Some(target) = target {
            entries::reselect(&self.rows, &mut self.list, Some(&target));
            self.list.center();
        }
        self.preview(cx);
    }

    /// Esc: the commit is closed and the history stands alone again, where it was.
    fn leave_commit(&mut self, cx: &mut TabContext) {
        let Some(opened) = self.opened.take() else {
            return;
        };
        self.files_focused = false;
        self.files_visible = false;
        self.opening = None;
        self.rebuild(cx.editor);
        // The history may have been read again meanwhile, so the commit is found by its hash.
        let index = self
            .listed()
            .iter()
            .position(|commit| commit.hash == opened.commit.hash);
        self.history_list.scroll = opened.list_scroll;
        self.history_list
            .select(index.unwrap_or(opened.list_cursor));
        // Back on the list, the diff goes on following the cursor, now over whole commits.
        self.follow = true;
        self.preview(cx);
    }

    /// Shows the diff of what the cursor is on, if there is one to show.
    fn preview(&mut self, cx: &mut TabContext) {
        if let Some(target) = self.diff_target() {
            let loader = cx.editor.syn_loader.load_full();
            cx.diff.ask(target, loader);
        }
    }

    /// With the files shown, the commit the cursor rests on in the history is the one they
    /// list: moving over the history opens it, without taking the keys from the history.
    fn follow_files(&mut self) {
        if !self.files_visible || self.files_focused {
            return;
        }
        let Some(commit) = self.listed().get(self.history_list.cursor).cloned() else {
            return;
        };
        let shown = self
            .opened
            .as_ref()
            .is_some_and(|opened| opened.commit.hash == commit.hash);
        if shown || self.opening.as_deref() == Some(commit.hash.as_str()) {
            return;
        }
        self.open_commit(commit, false);
    }

    /// Shows the diff of what the cursor is on once it has rested there: a burst of moves
    /// asks git for the last one only.
    fn preview_when_rested(&mut self) {
        self.moves = self.moves.wrapping_add(1);
        let move_number = self.moves;
        super::later(PREVIEW_DELAY, move |sidebar, editor| {
            if sidebar.commits.moves != move_number {
                return;
            }
            let mut cx = TabContext {
                editor,
                diff: &mut sidebar.diff,
            };
            sidebar.commits.follow_files();
            sidebar.commits.preview(&mut cx);
        });
    }

    /// The diff for the row under the cursor: a commit's whole patch in the history (that
    /// file's in a file's history); inside a commit, the whole commit on its own row, a
    /// directory's files on a directory, one file on a file.
    fn diff_target(&self) -> Option<DiffTarget> {
        let row = self.rows().get(self.list().cursor)?;
        let opened = self.opened.as_ref().filter(|_| self.files_focused);
        let Some(opened) = opened else {
            let Row::Commit(row) = row else {
                return None;
            };
            if !self.follow {
                return None;
            }
            let commit = self.listed().get(row.index)?;
            let (pathspecs, name) = match &commit.file {
                Some(file) => {
                    let mut pathspecs = vec![git::pathspec(file)];
                    if let Some(from) = &commit.file_from {
                        pathspecs.push(git::pathspec(from));
                    }
                    let base = file.rsplit('/').next().unwrap_or(file);
                    (pathspecs, format!("{} {base}", commit.short))
                }
                None => (vec![".".to_string()], commit.short.clone()),
            };
            return Some(DiffTarget {
                source: DiffSource::Commit(commit.hash.clone()),
                pathspecs,
                name,
                describe: true,
            });
        };
        let below_root = |path: &Path| {
            let inside = path.strip_prefix(&self.root).unwrap_or(path);
            git::pathspec(&format!("{}{}", opened.prefix, inside.to_string_lossy()))
        };
        let mut pathspecs = Vec::new();
        let mut name = opened.commit.short.clone();
        match row {
            Row::Symbol(_) => return None,
            Row::Commit(_) => {
                if !opened.prefix.is_empty() {
                    pathspecs.push(git::pathspec(&opened.prefix));
                }
            }
            Row::Entry(entry) if entry.is_dir => {
                pathspecs.push(below_root(&entry.path));
                name = format!("{name} {}/", entry.name);
            }
            Row::Entry(entry) => {
                pathspecs.push(below_root(&entry.path));
                name = format!("{name} {}", entry.name);
                let file = opened.files.iter().find(|file| file.path == entry.path);
                if let Some(from) = file.and_then(|file| file.from.as_ref()) {
                    pathspecs.push(git::pathspec(from));
                }
            }
        }
        // The commit's own row is the commit, message and all; a directory or a file of
        // it is read for itself, its heading naming it.
        Some(DiffTarget {
            source: DiffSource::Commit(opened.commit.hash.clone()),
            pathspecs,
            name,
            describe: matches!(row, Row::Commit(_)),
        })
    }
}

impl TabView for CommitsTab {
    fn label(&self) -> String {
        match &self.showing {
            Showing::Repository => "Commits".to_string(),
            Showing::File(path) => {
                let name = path.file_name().map(|name| name.to_string_lossy());
                format!("History {}", name.unwrap_or_default())
            }
        }
    }

    fn rows(&self) -> &[Row] {
        if self.files_focused {
            &self.rows
        } else {
            &self.history_rows
        }
    }

    fn list(&self) -> &List {
        if self.files_focused {
            &self.list
        } else {
            &self.history_list
        }
    }

    fn list_mut(&mut self) -> &mut List {
        if self.files_focused {
            &mut self.list
        } else {
            &mut self.history_list
        }
    }

    fn folds(&self) -> Option<&Folds> {
        self.opened.as_ref().map(|opened| &opened.folds)
    }

    fn folds_mut(&mut self) -> Option<&mut Folds> {
        self.opened.as_mut().map(|opened| &mut opened.folds)
    }

    fn empty_message(&self) -> Option<Message> {
        let (text, is_error) = match &self.log {
            None => ("reading git log…".to_string(), false),
            Some(Ok(_)) if self.filtered.is_some() => ("no commit matches".to_string(), false),
            Some(Ok(_)) => ("no commits".to_string(), false),
            Some(Err(err)) => (err.clone(), err != git::NOT_A_REPOSITORY),
        };
        Some(Message { text, is_error })
    }

    fn rebuild(&mut self, _editor: &mut Editor) {
        let selected = self
            .rows
            .get(self.list.cursor)
            .and_then(Row::path)
            .map(Path::to_path_buf);
        let mut rows = Vec::new();
        if let Some(opened) = &self.opened {
            rows.push(Row::Commit(commit_row(
                0,
                &opened.commit,
                author_width([&opened.commit]),
                true,
            )));
            self.layout = _editor.config().sidebar.commit_files;
            match self.layout {
                CommitFiles::Tree => {
                    entries::list_changed(&self.root, &opened.files, &opened.folds, &mut rows)
                }
                CommitFiles::Paths => entries::list_paths(&self.root, &opened.files, &mut rows),
            }
        }
        let filter = self.filter.as_deref().filter(|text| !text.is_empty());
        self.filtered = filter.map(|filter| {
            let filter = filter.to_lowercase();
            let source = match (&self.whole, &self.log) {
                (Some(whole), _) => whole.as_slice(),
                (None, Some(Ok(commits))) => commits.as_slice(),
                _ => &[],
            };
            source
                .iter()
                .filter(|commit| git::commit_matches(commit, &filter))
                .cloned()
                .collect()
        });
        let commits = self.listed();
        let author_width = author_width(commits);
        let history_rows = commits
            .iter()
            .enumerate()
            .map(|(index, commit)| Row::Commit(commit_row(index, commit, author_width, false)))
            .collect();
        self.history_rows = history_rows;
        self.history_list.set_len(self.history_rows.len());
        self.rows = rows;
        entries::reselect(&self.rows, &mut self.list, selected.as_deref());
    }

    fn shown(&mut self, _cx: &mut TabContext) {
        self.follow = false;
        let wants_fresh = self.showing == Showing::Repository && !self.armed;
        if self.log.is_none() || wants_fresh {
            self.ask_page(0);
        }
    }

    /// F5 reads the top of the history again, and the whole of it for an open filter; one that comes while a page is being read
    /// is kept for when it lands, never dropped.
    fn refresh(&mut self, _cx: &mut TabContext) {
        if self.filter.is_some() {
            self.read_whole();
        }
        if self.asking.is_some() {
            self.again = true;
            return;
        }
        self.ask_page(0);
    }

    fn cursor_moved(&mut self, _cx: &mut TabContext) {
        // Moving through the history is choosing a commit to look at, as a click is.
        if !self.files_focused {
            self.follow = true;
            self.ask_next_page_if_near_end();
        }
        self.preview_when_rested();
    }

    fn step_back(&mut self, cx: &mut TabContext) -> bool {
        if self.opened.is_some() {
            self.leave_commit(cx);
            return true;
        }
        if self.showing != Showing::Repository {
            self.set_showing(Showing::Repository);
            self.rebuild(cx.editor);
            self.ask_page(0);
            return true;
        }
        false
    }

    fn open(&mut self, cx: &mut TabContext, how: Activation) -> Outcome {
        let Some(row) = self.rows().get(self.list().cursor) else {
            return Outcome::Stay;
        };
        let in_history = !self.files_focused;
        match (row, how) {
            // Enter or a double click on a commit opens the files it touched under the
            // history, and pressed again closes them: the same pane F9 shows, for the
            // commit the cursor is on. The keys stay in the history, so the next Enter
            // is the one that closes, and the arrows go on over the commits; Alt-Down
            // takes the keys into the files.
            (Row::Commit(_), Activation::Enter | Activation::Double) if in_history => {
                self.set_files_visible(cx, !self.files_visible, false);
                Outcome::Stay
            }
            (Row::Commit(row), _) if in_history => {
                if let Some(commit) = self.listed().get(row.index).cloned() {
                    self.open_commit(commit, false);
                }
                Outcome::Stay
            }
            // Inside a commit the diff already follows the cursor: a click only moves it,
            // Enter goes over to read it, asked again in case the buffer was left meanwhile.
            (_, Activation::Click) => {
                self.preview(cx);
                Outcome::Stay
            }
            (_, Activation::Enter | Activation::Double) => {
                cx.diff.forget();
                self.preview(cx);
                Outcome::Leave
            }
        }
    }
}

fn commit_row(index: usize, commit: &Commit, author_width: usize, head: bool) -> CommitRow {
    CommitRow {
        index,
        short: commit.short.clone(),
        subject: commit.subject.clone(),
        time: commit.time,
        date: commit.date.clone(),
        author: commit.author.clone(),
        author_width,
        head,
    }
}

/// The columns the longest of these authors takes, up to `AUTHOR_MAX_WIDTH`.
fn author_width<'a>(commits: impl IntoIterator<Item = &'a Commit>) -> usize {
    commits
        .into_iter()
        .map(|commit| commit.author.width())
        .max()
        .unwrap_or(0)
        .min(AUTHOR_MAX_WIDTH)
}

/// Draws a commit on one line. A list wide enough reads as `git log` does: hash, date,
/// author and subject; a narrower one shows the subject and its age at the right edge,
/// with the hash only on the opened commit's own row.
pub fn draw_commit(surface: &mut Surface, paint: &RowPaint, row: &CommitRow, theme: &Theme) {
    let text_style = theme.get("ui.text");
    // The selection's background can be the dimmed colour itself, so a selected row draws
    // its hash and age in the text's colour.
    let mut dim_style = if paint.selected.is_some() {
        text_style
    } else {
        theme.get("ui.text.inactive")
    };
    let mut subject_style = text_style;
    if row.head {
        dim_style = dim_style.add_modifier(Modifier::BOLD);
        subject_style = subject_style.add_modifier(Modifier::BOLD);
    }
    if let Some(selected) = paint.selected {
        dim_style = dim_style.patch(selected);
        subject_style = subject_style.patch(selected);
    }
    let column_style = |scope: &str, fallback: &str| {
        let mut style = theme.try_get(scope).unwrap_or_else(|| theme.get(fallback));
        if row.head {
            style = style.add_modifier(Modifier::BOLD);
        }
        if let Some(selected) = paint.selected {
            style = style.patch(selected);
        }
        style
    };
    let hash_style = column_style("ui.commit.hash", "type");
    let date_style = column_style("ui.commit.date", "constant.numeric");
    let author_style = column_style("ui.commit.author", "diff.plus");
    let x = paint.line.x + 1;
    let y = paint.line.y;
    let width = (paint.line.width as usize).saturating_sub(2);
    let paint_subject = |_: usize| -> Style { subject_style };
    let wide_prefix = row.short.width() + 1 + row.date.width() + 1 + row.author_width + 1;
    if width >= wide_prefix + WIDE_SUBJECT_WIDTH {
        let (after_hash, _) = surface.set_stringn(x, y, &row.short, width, hash_style);
        let (after_date, _) =
            surface.set_stringn(after_hash + 1, y, &row.date, row.date.width(), date_style);
        let author_x = after_date + 1;
        surface.set_string_truncated(
            author_x,
            y,
            &row.author,
            row.author_width,
            |_| author_style,
            true,
            false,
        );
        let subject_x = x + wide_prefix as u16;
        surface.set_string_truncated(
            subject_x,
            y,
            &row.subject,
            width - wide_prefix,
            paint_subject,
            true,
            false,
        );
        return;
    }
    let age = format_age(row.time);
    let mut subject_x = x;
    if row.head {
        let (after_hash, _) = surface.set_stringn(x, y, &row.short, width, hash_style);
        subject_x = after_hash + 1;
    }
    let used = (subject_x - x) as usize;
    let subject_width = width.saturating_sub(used + age.len() + 1);
    surface.set_string_truncated(
        subject_x,
        y,
        &row.subject,
        subject_width,
        paint_subject,
        true,
        false,
    );
    if width >= used + age.len() {
        let age_x = x + (width - age.len()) as u16;
        surface.set_string(age_x, y, &age, dim_style);
    }
}

/// How long ago a commit was made, in as few characters as still read: `5m`, `3h`, `2d`.
pub fn format_age(time: i64) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    const MONTH: i64 = 30 * DAY;
    const YEAR: i64 = 365 * DAY;

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(time, |since| since.as_secs() as i64);
    let seconds = (now - time).max(0);
    let (amount, unit) = match seconds {
        s if s < HOUR => (s / MINUTE, "m"),
        s if s < DAY => (s / HOUR, "h"),
        s if s < MONTH => (s / DAY, "d"),
        s if s < YEAR => (s / MONTH, "mo"),
        s => (s / YEAR, "y"),
    };
    format!("{amount}{unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panes_keep_independent_positions_and_diff_targets() {
        let mut tab = CommitsTab::new(PathBuf::from("/repo"));
        let commit = Commit {
            hash: "abc123".into(),
            short: "abc123".into(),
            time: 0,
            date: "1970-01-01 00:00".into(),
            author: "Someone".into(),
            subject: "A commit".into(),
            file: None,
            file_from: None,
        };
        let row = || Row::Commit(commit_row(0, &commit, 7, false));
        tab.history_rows = vec![row(), row()];
        tab.rows = vec![row()];
        tab.history_list.set_len(2);
        tab.list.set_len(1);
        tab.history_list.select(1);
        tab.log = Some(Ok(vec![commit.clone()]));
        tab.opened = Some(OpenCommit {
            commit,
            prefix: String::new(),
            files: vec![],
            folds: Folds::opened(),
            list_cursor: 1,
            list_scroll: 0,
        });
        tab.set_pages(1, 3);
        assert!(!tab.files_visible());
        assert!(!tab.has_files());
        tab.focus_files(true);
        assert!(!tab.files_focused);
        tab.files_visible = true;
        assert_eq!(tab.history_list.page, 1);
        assert_eq!(tab.list.page, 3);
        tab.focus_files(true);
        assert_eq!(tab.rows().len(), 1);
        assert_eq!(tab.list().cursor, 0);
        assert_eq!(
            tab.diff_target().unwrap().source,
            DiffSource::Commit("abc123".into())
        );
        tab.focus_files(false);
        assert_eq!(tab.rows().len(), 2);
        assert_eq!(tab.list().cursor, 1);
        assert!(tab.has_files());
        assert_eq!(tab.panes()[1].0.len(), 1);
        assert!(tab.diff_target().is_none());
        tab.follow = true;
        assert_eq!(
            tab.diff_target().unwrap().source,
            DiffSource::Commit("abc123".into())
        );
        tab.set_showing(Showing::Repository);
        assert!(!tab.has_files());
        assert!(!tab.files_focused);
    }
}
