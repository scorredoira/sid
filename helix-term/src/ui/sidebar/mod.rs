//! The sidebar: a column left of the editor with tabs on what a project is — the files on
//! disk, what git sees changed, the history. The sidebar owns what the tabs share: the
//! keys, the mouse, the cursor's movement, the folding of directories, the drawing, and
//! the buffer diffs are shown in. A tab is a [`TabView`]; adding one is a file and a
//! variant of [`TabKind`].
//!
//! Nothing is asked of git or the disk while drawing: a tab asks through [`background`]
//! and the answer lands back in the sidebar on the main thread, as a job of the editor's.

pub mod changes;
mod commit_layout;
pub mod commits;
pub mod diff_view;
pub mod entries;
pub mod files;
pub mod git;
pub mod list;
pub mod outline;
mod review;
pub mod tab;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use helix_view::graphics::{Modifier, Rect};
use helix_view::input::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use helix_view::keyboard::{KeyCode, KeyModifiers};
use helix_view::Editor;
use tui::buffer::Buffer as Surface;

use crate::commands;
use crate::compositor::EventResult;
use crate::ui::editor;
use crate::ui::panel_width;
use crate::ui::{context_menu, settings};

use changes::{Act, ChangesTab};
use commit_layout::CommitLayout;
use commits::CommitsTab;
use diff_view::DiffView;
use entries::{Row, RowPaint};
use files::{FilesTab, PromptTarget};
use outline::Outline;
use tab::{Activation, Outcome, TabContext, TabView};

pub use commits::format_age;
pub use git::Commit;

/// How long a tab that asks git waits after an answer before asking again, while it is on
/// screen.
pub const REFRESH: Duration = Duration::from_secs(2);

/// Two clicks on the same row closer than this are a double click: the terminal reports
/// each press on its own, so the sidebar tells them apart itself.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);

/// How long a letter typed in the tree waits for the next one before it starts a new name.
const TYPING: Duration = Duration::from_millis(800);

/// The narrowest the separator can be dragged to.
const MIN_WIDTH: u16 = 12;

/// Where the width the separator was dragged to is remembered.
const WIDTH_FILE: &str = "sidebar";

/// The columns the sidebar always leaves to the editor, however wide it is asked to be.
pub const EDITOR_ROOM: u16 = 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TabKind {
    Files,
    Changes,
    Commits,
}

impl TabKind {
    /// The tabs in the order of the strip, which Tab walks.
    const ALL: [TabKind; 3] = [TabKind::Files, TabKind::Changes, TabKind::Commits];

    /// Whether the tab reads git, and has nothing to show outside a repository.
    fn needs_git(self) -> bool {
        self != TabKind::Files
    }
}

pub struct Sidebar {
    root: PathBuf,
    /// Whether the workspace is in a git repository; outside one only Files is offered.
    in_git: bool,
    tab: TabKind,
    files: FilesTab,
    changes: ChangesTab,
    commits: CommitsTab,
    /// The definitions of the file being edited, under the tree in the Files tab.
    outline: Outline,
    diff: DiffView,
    pub open: bool,
    pub focused: bool,
    code_hidden: bool,
    /// Whether the first render has laid the rows out; before it there is no editor to ask.
    built: bool,
    /// The document the sidebar last moved onto, so a buffer switch is noticed at render.
    revealed: Option<PathBuf>,
    area: Rect,
    /// Where each tab's label was drawn on the strip, for a click to land on.
    tab_columns: [(u16, u16); TabKind::ALL.len()],
    /// Where the outline's order was written on its header, for a click to change it.
    sort_columns: (u16, u16),
    /// The width the separator was dragged to, kept between sessions over the configured.
    width: Option<u16>,
    /// Whether the separator is being dragged, so the mouse is the sidebar's wherever it goes.
    resizing: bool,
    resizing_split: bool,
    commit_layout: CommitLayout,
    /// Why the remembered width could not be read, said on the first render: at startup the
    /// editor's own messages would cover it.
    width_error: Option<String>,
    /// The row last clicked and when, so a second click on it soon after is a double click.
    last_click: Option<(usize, Instant)>,
    /// What has been typed to walk to a row by name, and when the last letter landed.
    typed: (String, Option<Instant>),
}

/// Whether a key is a shortcut of the editor's rather than one of the sidebar's. A key
/// held with Ctrl, Alt or Cmd, and a function key, are shortcuts wherever the focus is;
/// a plain letter is not, or it would run a command on the file behind the sidebar.
pub(crate) fn is_editor_shortcut(key: KeyEvent) -> bool {
    if matches!(key.code, KeyCode::F(_)) {
        return true;
    }

    key.modifiers
        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
}

impl Sidebar {
    /// Shows the files that start with a dot in the tree, or hides them, and writes the
    /// choice to `config.toml` as the settings screen would.
    pub fn toggle_hidden(&mut self, editor: &mut Editor) {
        let hidden = !self.files.hidden(editor);
        self.files.set_hidden(editor, hidden);
        let key = "file-explorer.hidden";
        let value = serde_json::Value::Bool(hidden);
        if let Err(err) = settings::apply(editor, key, &value) {
            log::error!("Could not change '{key}': {err:#}");
        }
        if let Err(err) = settings::write_setting(&helix_loader::config_file(), key, &value) {
            log::error!("Could not write '{key}' to config.toml: {err:#}");
            editor.set_error(format!("Changed, but not written down: {err:#}"));
            return;
        }
        editor.set_status(if hidden {
            "Hidden files are hidden"
        } else {
            "Hidden files are shown"
        });
    }

    pub fn new(root: PathBuf, open: bool) -> Self {
        // A broken file costs the remembered width, not the sidebar.
        let (width, width_error) = match panel_width::load(WIDTH_FILE) {
            Ok(width) => (width, None),
            Err(err) => {
                log::error!("Could not read the sidebar's width: {err:#}");
                let message = format!("Could not read the sidebar's width: {err:#}");
                (None, Some(message))
            }
        };
        let (commit_layout, layout_error) = match CommitLayout::load() {
            Ok(layout) => (layout, None),
            Err(err) => {
                log::error!("Could not read the commit layout: {err:#}");
                (
                    CommitLayout::default(),
                    Some(format!("Could not read the commit layout: {err:#}")),
                )
            }
        };
        let (outline, outline_error) = Outline::new();
        Self {
            files: FilesTab::new(root.clone()),
            changes: ChangesTab::new(root.clone()),
            commits: CommitsTab::new(root.clone()),
            outline,
            diff: DiffView::new(root.clone()),
            in_git: git::inside_repository(&root),
            root,
            tab: TabKind::Files,
            open,
            focused: false,
            code_hidden: false,
            built: false,
            revealed: None,
            area: Rect::default(),
            tab_columns: [(0, 0); TabKind::ALL.len()],
            sort_columns: (0, 0),
            width,
            resizing: false,
            resizing_split: false,
            commit_layout,
            width_error: width_error.or(layout_error).or(outline_error),
            last_click: None,
            typed: (String::new(), None),
        }
    }

    /// Commits starts at half the terminal, capped for wide monitors; explicit drags win.
    pub fn width(&self, configured: u16, screen_width: u16) -> u16 {
        if self.tab == TabKind::Commits {
            self.commit_layout.width(screen_width)
        } else {
            self.width.unwrap_or(configured)
        }
    }

    /// The two panes of a tab that stacks them: the history over a commit's files, the
    /// tree over the outline.
    fn stacked_areas(&self) -> Option<[Rect; 2]> {
        match self.tab {
            TabKind::Commits if self.commits.has_files() => self.commit_layout.panes(self.area),
            TabKind::Files if self.outline.shown() => self.outline.panes(self.area),
            _ => None,
        }
    }

    /// Whether the lower of two stacked panes has the keys.
    fn lower_focused(&self) -> bool {
        match self.tab {
            TabKind::Commits => self.commits.files_focused(),
            TabKind::Files => self.outline.focused,
            TabKind::Changes => false,
        }
    }

    fn focus_lower(&mut self, lower: bool) {
        match self.tab {
            TabKind::Commits => self.commits.focus_files(lower),
            TabKind::Files => self.outline.focused = lower && self.outline.shown(),
            TabKind::Changes => {}
        }
        self.last_click = None;
    }

    fn active_area(&self) -> Rect {
        match self.stacked_areas() {
            Some(panes) => panes[usize::from(self.lower_focused())],
            None => self.area,
        }
    }

    /// Whether the outline is on screen, under the tree.
    pub fn outline_visible(&self) -> bool {
        self.showing(TabKind::Files) && self.outline.shown()
    }

    pub fn outline_by_name(&self) -> bool {
        self.outline.by_name()
    }

    /// Hides the outline when it is on screen; otherwise brings the tree on screen with
    /// the outline under it. The keys stay in the text: Ctrl-E takes them to the outline.
    pub fn toggle_outline(&mut self, editor: &mut Editor) {
        if self.outline_visible() {
            self.outline.set_shown(false);
            if self.focused {
                self.focus_code();
            }
            return;
        }
        self.show_files(editor);
        self.outline.set_shown(true);
        self.outline.sync(editor, false);
    }

    /// A language server answered the outline's question about a file.
    pub(crate) fn outline_landed(
        &mut self,
        editor: &mut Editor,
        key: (helix_view::DocumentId, usize),
        said: Option<Vec<outline::Said>>,
    ) {
        self.outline.landed(editor, key, said);
    }

    /// Lists the outline by name, or back in the file's order.
    pub fn toggle_outline_sort(&mut self, editor: &mut Editor) {
        self.outline.toggle_sort(editor);
        editor.set_status(format!("Outline {}", self.outline.sort_label()));
    }

    pub fn resizing(&self) -> bool {
        self.resizing || self.resizing_split
    }

    /// Whether `kind` is the tab on screen.
    pub fn showing(&self, kind: TabKind) -> bool {
        self.open && self.tab == kind
    }

    pub fn code_hidden(&self) -> bool {
        self.showing(TabKind::Commits) && self.code_hidden
    }

    pub fn focus_code(&mut self) {
        self.code_hidden = false;
        self.focused = false;
    }

    /// The tabs offered: Changes and Commits only inside a git repository.
    fn tabs(&self) -> impl Iterator<Item = TabKind> + '_ {
        TabKind::ALL
            .into_iter()
            .filter(|kind| self.in_git || !kind.needs_git())
    }

    /// Looks again whether the workspace is in a repository, one made or removed since, and
    /// leaves a git tab that no longer has one for Files.
    fn check_git(&mut self) {
        self.in_git = git::inside_repository(&self.root);
        if !self.in_git && self.tab.needs_git() {
            self.tab = TabKind::Files;
            self.code_hidden = false;
        }
    }

    /// Whether a git tab can be shown; outside a repository says so instead.
    fn git_available(&mut self, editor: &mut Editor) -> bool {
        self.check_git();
        if !self.in_git {
            editor.set_status(format!(
                "{} is not in a git repository",
                self.root.display()
            ));
        }
        self.in_git
    }

    pub fn toggle_commits(&mut self, editor: &mut Editor) {
        if !self.showing(TabKind::Commits) && !self.git_available(editor) {
            return;
        }
        if self.showing(TabKind::Commits) {
            self.open = false;
            self.focus_code();
        } else {
            self.open = true;
            self.focused = true;
            self.code_hidden = false;
            self.tab = TabKind::Commits;
            self.came_on_screen(editor);
        }
    }

    pub fn files_visible(&self) -> bool {
        self.commits.files_visible()
    }

    pub fn toggle_commit_files(&mut self, editor: &mut Editor) {
        if !self.git_available(editor) {
            return;
        }
        if !self.showing(TabKind::Commits) {
            self.toggle_commits(editor);
        }
        let mut cx = TabContext {
            editor,
            diff: &mut self.diff,
        };
        self.commits.toggle_files(&mut cx);
    }

    pub fn toggle_code(&mut self, editor: &mut Editor) {
        let hide = !self.code_hidden();
        if hide && !self.git_available(editor) {
            return;
        }
        if hide && !self.showing(TabKind::Commits) {
            self.toggle_commits(editor);
        }
        self.code_hidden = hide;
        self.focused = hide;
    }

    pub fn toggle_context(&mut self, editor: &mut Editor) {
        self.focus_code();
        self.diff.toggle_context(editor);
    }

    pub fn full_context(&self) -> bool {
        self.diff.full_context()
    }

    pub fn toggle_side_by_side(&mut self, editor: &mut Editor) {
        self.focus_code();
        self.diff.toggle_side_by_side(editor);
    }

    pub fn side_by_side(&self) -> bool {
        self.diff.side_by_side()
    }

    /// Keeps the two sides of a diff shown side by side on the same rows; run before the
    /// views are drawn, whether the sidebar is open or not.
    pub fn follow_diff(&mut self, editor: &mut Editor) {
        self.diff.follow(editor);
    }

    pub fn toggle(&mut self, editor: &mut Editor) {
        self.open = !self.open;
        if self.open {
            self.came_on_screen(editor);
        } else {
            self.code_hidden = false;
            self.focused = false;
        }
    }

    pub fn focus(&mut self, editor: &mut Editor) {
        let was_open = self.open;
        self.open = true;
        self.focused = true;
        if !was_open {
            self.came_on_screen(editor);
        }
        // The render moves onto the current file, once it knows how many rows fit.
        self.revealed = None;
    }

    /// Shows the Files tab, focused, on the file being edited.
    pub fn reveal(&mut self, editor: &mut Editor) {
        let was_showing = self.showing(TabKind::Files);
        self.open = true;
        self.focused = true;
        self.tab = TabKind::Files;
        if !was_showing {
            self.came_on_screen(editor);
        }
        // The render moves onto the current file, once it knows how many rows fit.
        self.revealed = None;
    }

    /// Shows the history of one file in the Commits tab, focused.
    pub fn show_history(&mut self, editor: &mut Editor, path: PathBuf) {
        if !path.starts_with(&self.root) {
            editor.set_error(format!("{} is outside the workspace", path.display()));
            return;
        }
        if !self.git_available(editor) {
            return;
        }
        self.open = true;
        self.focused = true;
        self.tab = TabKind::Commits;
        self.revealed = None;
        let mut cx = TabContext {
            editor,
            diff: &mut self.diff,
        };
        self.commits.show_history(&mut cx, path);
    }

    /// The diff buffer's line under the cursor as a line to blame, and the folder to ask git
    /// in, when the focused view shows the diff buffer on a line of code.
    pub fn diff_blame_request(&self, editor: &Editor) -> Option<(PathBuf, git::BlameRequest)> {
        let request = self.diff.blame_request(editor)?;
        Some((self.root.clone(), request))
    }

    /// Opens `commit` into its files in the Commits tab, focused.
    pub fn open_commit(&mut self, commit: Commit) {
        self.open = true;
        self.focused = true;
        self.tab = TabKind::Commits;
        self.revealed = None;
        self.commits.open_commit(commit);
    }

    /// Something on disk changed under `path`, by one of the sidebar's own prompts.
    pub fn disk_changed(&mut self, editor: &mut Editor, path: &Path) {
        self.files.disk_changed(editor, path);
        self.changes.ask();
    }

    /// The tab on screen, and the diff buffer beside it, borrowed apart so a tab can act on
    /// both.
    fn parts(&mut self) -> (&mut dyn TabView, &mut DiffView) {
        let tab: &mut dyn TabView = match self.tab {
            TabKind::Files if self.outline.focused => &mut self.outline,
            TabKind::Files => &mut self.files,
            TabKind::Changes => &mut self.changes,
            TabKind::Commits => &mut self.commits,
        };
        (tab, &mut self.diff)
    }

    /// The list that has the keys: the tab on screen, or the outline under the tree.
    fn active(&self) -> &dyn TabView {
        match self.tab {
            TabKind::Files if self.outline.focused => &self.outline,
            TabKind::Files => &self.files,
            TabKind::Changes => &self.changes,
            TabKind::Commits => &self.commits,
        }
    }

    fn active_mut(&mut self) -> &mut dyn TabView {
        self.parts().0
    }

    /// The tab on screen itself, whichever of its panes has the keys.
    fn tab_view(&self) -> &dyn TabView {
        match self.tab {
            TabKind::Files => &self.files,
            TabKind::Changes => &self.changes,
            TabKind::Commits => &self.commits,
        }
    }

    fn tab_parts(&mut self) -> (&mut dyn TabView, &mut DiffView) {
        let tab: &mut dyn TabView = match self.tab {
            TabKind::Files => &mut self.files,
            TabKind::Changes => &mut self.changes,
            TabKind::Commits => &mut self.commits,
        };
        (tab, &mut self.diff)
    }

    /// The tab on screen was just put there: it lays itself out and asks what it asks.
    fn came_on_screen(&mut self, editor: &mut Editor) {
        if !self.built {
            // Said from a job of the editor's, not from the render that first needs it.
            later(Duration::ZERO, |sidebar, editor| {
                if let Some(err) = sidebar.width_error.take() {
                    editor.set_error(err);
                }
            });
        }
        self.built = true;
        self.check_git();
        let (tab, diff) = self.tab_parts();
        tab.rebuild(editor);
        let mut cx = TabContext { editor, diff };
        tab.shown(&mut cx);
        self.revealed = None;
    }

    fn switch_tab(&mut self, kind: TabKind, editor: &mut Editor) {
        if self.tab == kind {
            // Asking for the tab again steps back, where the tab has somewhere to go.
            let (tab, diff) = self.parts();
            let mut cx = TabContext { editor, diff };
            tab.step_back(&mut cx);
            return;
        }
        self.code_hidden = false;
        self.tab = kind;
        self.came_on_screen(editor);
    }

    /// Moves the tab on screen onto the focused document, if it shows files. Called at
    /// render, where the rows that fit are known, so the file lands mid-screen.
    fn reveal_current(&mut self, editor: &mut Editor) {
        let current = doc!(editor).path().map(Path::to_path_buf);
        self.revealed = current.clone();
        if let Some(path) = current {
            self.tab_parts().0.reveal(editor, &path);
        }
    }

    fn cursor_moved(&mut self, editor: &mut Editor) {
        let (tab, diff) = self.parts();
        let mut cx = TabContext { editor, diff };
        tab.cursor_moved(&mut cx);
    }

    /// Opens the row under the cursor: a directory is folded or unfolded, anything else is
    /// the tab's to open. Returns whether the keys go over to the editor.
    fn open_row(&mut self, editor: &mut Editor, how: Activation) -> bool {
        let tab = self.active();
        let row = tab.rows().get(tab.list().cursor);
        if row.is_some_and(|row| row.dir().is_some()) {
            self.toggle_dir(editor);
            return false;
        }
        let (tab, diff) = self.parts();
        let mut cx = TabContext { editor, diff };
        let outcome = tab.open(&mut cx, how);
        if outcome == Outcome::Leave {
            // The tab just opened what is now the focused document: nothing to move onto.
            self.revealed = doc!(editor).path().map(Path::to_path_buf);
        }
        outcome == Outcome::Leave
    }

    fn toggle_dir(&mut self, editor: &mut Editor) {
        let tab = self.active();
        let Some(dir) = tab.rows().get(tab.list().cursor).and_then(Row::dir) else {
            return;
        };
        let dir = dir.to_path_buf();
        let open = tab.folds().is_some_and(|folds| folds.is_open(&dir));
        self.set_dir_open(editor, dir, !open);
    }

    fn expand_dir(&mut self, editor: &mut Editor) {
        let tab = self.active();
        let Some(dir) = tab.rows().get(tab.list().cursor).and_then(Row::dir) else {
            return;
        };
        let dir = dir.to_path_buf();
        if tab.folds().is_some_and(|folds| folds.is_open(&dir)) {
            return;
        }
        self.set_dir_open(editor, dir, true);
    }

    fn set_dir_open(&mut self, editor: &mut Editor, dir: PathBuf, open: bool) {
        let tab = self.active_mut();
        let Some(folds) = tab.folds_mut() else {
            return;
        };
        folds.set(dir, open);
        tab.rebuild(editor);
    }

    fn collapse_all(&mut self, editor: &mut Editor) {
        let tab = self.active_mut();
        let dirs: Vec<PathBuf> = tab
            .rows()
            .iter()
            .filter_map(Row::dir)
            .map(Path::to_path_buf)
            .collect();
        let Some(folds) = tab.folds_mut() else {
            return;
        };
        folds.close_all(dirs.into_iter());
        tab.rebuild(editor);
    }

    /// Folds every directory of the file tree, whichever tab is on screen and wherever the
    /// focus is.
    pub fn collapse_files(&mut self, editor: &mut Editor) {
        let dirs: Vec<PathBuf> = self
            .files
            .rows()
            .iter()
            .filter_map(Row::dir)
            .map(Path::to_path_buf)
            .collect();
        if let Some(folds) = self.files.folds_mut() {
            folds.close_all(dirs.into_iter());
        }
        // Before the first render there are no rows to lay out; showing the tab builds them.
        if self.built {
            self.files.rebuild(editor);
        }
    }

    /// Closes the directory under the cursor; on a file or a closed directory, jumps to
    /// the parent instead, so repeated presses walk up the tree.
    fn collapse_or_parent(&mut self, editor: &mut Editor) {
        let tab = self.active();
        let cursor = tab.list().cursor;
        let Some(row) = tab.rows().get(cursor) else {
            return;
        };
        if let Some(dir) = row.dir() {
            if tab.folds().is_some_and(|folds| folds.is_open(dir)) {
                let dir = dir.to_path_buf();
                self.set_dir_open(editor, dir, false);
                return;
            }
        }
        let depth = row.depth();
        if depth == 0 {
            return;
        }
        let parent = tab.rows()[..cursor]
            .iter()
            .rposition(|candidate| candidate.depth() < depth);
        if let Some(parent) = parent {
            self.active_mut().list_mut().select(parent);
        }
    }

    /// Where a prompt acts: the workspace, and the entry under the cursor.
    fn prompt_target(&self) -> PromptTarget {
        let tab = self.active();
        let entry = tab.rows().get(tab.list().cursor).and_then(Row::entry);
        PromptTarget {
            root: self.root.clone(),
            path: entry.map(|entry| entry.path.clone()),
            is_dir: entry.is_some_and(|entry| entry.is_dir),
        }
    }

    /// What a shortcut that reaches the tree from anywhere acts on: the row the Files tab
    /// has selected when it is on screen with one, else the file being edited. So New,
    /// Rename and Delete mean the same thing whether the focus is in the tree or in the
    /// code, which is the whole point of their having their own keys.
    pub fn target_anywhere(&self, editor: &Editor) -> files::PromptTarget {
        if self.open && self.tab == TabKind::Files {
            let target = self.prompt_target();
            if target.path.is_some() {
                return target;
            }
        }
        files::PromptTarget {
            root: self.root.clone(),
            path: doc!(editor).path().map(Path::to_path_buf),
            is_dir: false,
        }
    }

    /// Brings the tree on screen, without taking the focus: a shortcut that acts on a row
    /// shows which row it acted on.
    pub fn show_files(&mut self, editor: &mut Editor) {
        if !self.open {
            self.toggle(editor);
        }
        if self.tab != TabKind::Files {
            self.switch_tab(TabKind::Files, editor);
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, cx: &mut commands::Context) -> EventResult {
        let editor = &mut cx.editor;
        // Inside a commit's files the keys are the files' own; the filter is the history's.
        // The outline has no filter box: typing walks it as it walks the tree.
        let filters = !self.lower_focused() && self.tab != TabKind::Changes;
        if filters {
            if let Some(result) = self.handle_filter_key(key, editor) {
                return result;
            }
        }
        if self.tab == TabKind::Changes {
            if let Some(result) = self.handle_changes_key(key, cx) {
                return result;
            }
        }
        if self.stacked_areas().is_some()
            && key.modifiers == KeyModifiers::ALT
            && matches!(key.code, KeyCode::Up | KeyCode::Down)
        {
            self.focus_lower(key.code == KeyCode::Down);
            return EventResult::Consumed(None);
        }
        let editor = &mut cx.editor;
        let before = self.active().list().cursor;
        let page = self.active().list().page as isize;
        match (key.code, key.modifiers) {
            (KeyCode::Esc, _) => {
                let (tab, diff) = self.parts();
                let mut tab_cx = TabContext { editor, diff };
                if !tab.step_back(&mut tab_cx) {
                    self.code_hidden = false;
                    self.focused = false;
                }
            }
            (KeyCode::Down, _) => {
                self.active_mut().list_mut().move_by(1);
            }
            (KeyCode::Up, _) => {
                self.active_mut().list_mut().move_by(-1);
            }
            (KeyCode::PageDown, _) => {
                self.active_mut().list_mut().move_by(page);
            }
            (KeyCode::PageUp, _) => {
                self.active_mut().list_mut().move_by(-page);
            }
            (KeyCode::Home, _) => {
                self.active_mut().list_mut().home();
            }
            (KeyCode::End, _) => {
                self.active_mut().list_mut().end();
            }
            (KeyCode::Enter, _) => {
                if self.open_row(editor, Activation::Enter) {
                    self.code_hidden = false;
                    self.focused = false;
                }
            }
            (KeyCode::Right, _) => {
                let tab = self.active();
                let on_dir = tab
                    .rows()
                    .get(tab.list().cursor)
                    .is_some_and(|row| row.dir().is_some());
                if on_dir {
                    self.expand_dir(editor);
                } else if self.open_row(editor, Activation::Enter) {
                    self.code_hidden = false;
                    self.focused = false;
                }
            }
            (KeyCode::Left, KeyModifiers::SHIFT) => {
                self.collapse_all(editor);
            }
            (KeyCode::Left, _) => {
                self.collapse_or_parent(editor);
            }
            (KeyCode::F(5), KeyModifiers::NONE) => {
                if self.tab.needs_git() {
                    self.check_git();
                }
                let (tab, diff) = self.parts();
                let mut tab_cx = TabContext { editor, diff };
                tab.refresh(&mut tab_cx);
            }
            (KeyCode::Tab, _) => {
                self.check_git();
                let tabs: Vec<TabKind> = self.tabs().collect();
                let index = tabs.iter().position(|kind| *kind == self.tab);
                let next = tabs[index.map_or(0, |index| (index + 1) % tabs.len())];
                if next != self.tab {
                    self.switch_tab(next, editor);
                }
            }
            // Delete is the tree's own: there is no text here for it to take a character
            // from, so it shadows nothing. New and rename are shortcuts that reach the
            // tree from anywhere, so they are not bound a second time here.
            (KeyCode::Delete, _) if self.active().edits_disk() => {
                let target = self.prompt_target();
                files::prompt_delete(cx, target);
            }
            (KeyCode::Char('.'), KeyModifiers::NONE) if self.active().edits_disk() => {
                self.toggle_hidden(editor);
            }
            // Anything the sidebar does not use but the editor might: it goes through, so
            // Ctrl-q quits and Ctrl-s saves wherever the focus is.
            _ if is_editor_shortcut(key) => return EventResult::Ignored(None),
            // A letter is not a command here: typing walks to the row that starts with what
            // you typed, the way a file explorer does.
            (KeyCode::Char(char), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                self.jump_to_typed(char);
            }
            _ => {}
        }
        if self.active().list().cursor != before {
            self.cursor_moved(cx.editor);
        }
        EventResult::Consumed(None)
    }

    /// The filter box of the Files and Commits tabs: `/` opens it, what is typed narrows
    /// the rows, Backspace takes a letter back, and Esc clears it and brings the whole tree
    /// or history back. Answers only for the keys the box takes. The key carries no
    /// modifier, so nothing the editor's shortcuts do is shadowed while the tree has the
    /// focus; once the box is open a typed `/` is text like any other letter.
    fn handle_filter_key(&mut self, key: KeyEvent, editor: &mut Editor) -> Option<EventResult> {
        let text = self.filter().map(str::to_string);
        match (key.code, key.modifiers, text) {
            (KeyCode::Char('/'), KeyModifiers::NONE, None) => {
                self.set_filter(editor, Some(String::new()));
            }
            (KeyCode::Esc, _, Some(_)) => {
                self.set_filter(editor, None);
                // The folds are back as they were, so the file opened from a match is
                // revealed again at the next render.
                if self.tab == TabKind::Files {
                    self.revealed = None;
                }
            }
            (KeyCode::Backspace, _, Some(mut text)) => {
                text.pop();
                self.set_filter(editor, Some(text));
            }
            (KeyCode::Char(char), KeyModifiers::NONE | KeyModifiers::SHIFT, Some(mut text)) => {
                text.push(char);
                self.set_filter(editor, Some(text));
            }
            _ => return None,
        }
        Some(EventResult::Consumed(None))
    }

    /// The active tab's filter box, when it has one and it is open.
    fn filter(&self) -> Option<&str> {
        match self.tab {
            TabKind::Files => self.files.filter(),
            TabKind::Commits => self.commits.filter(),
            TabKind::Changes => None,
        }
    }

    fn set_filter(&mut self, editor: &mut Editor, text: Option<String>) {
        match self.tab {
            TabKind::Files => self.files.set_filter(editor, text),
            TabKind::Commits => self.commits.set_filter(editor, text),
            TabKind::Changes => {}
        }
    }

    /// What the Changes tab does to the file under the cursor: `s` stages it, `u` takes it
    /// out of the index, `d` or Delete throws its working changes away, after asking.
    fn handle_changes_key(
        &mut self,
        key: KeyEvent,
        cx: &mut commands::Context,
    ) -> Option<EventResult> {
        if (key.code, key.modifiers) == (KeyCode::Char('o'), KeyModifiers::NONE) {
            if self.changes.open_file(cx.editor) {
                self.code_hidden = false;
                self.focused = false;
            }
            return Some(EventResult::Consumed(None));
        }
        let act = match (key.code, key.modifiers) {
            (KeyCode::Char('s'), KeyModifiers::NONE) => Act::Stage,
            (KeyCode::Char('u'), KeyModifiers::NONE) => Act::Unstage,
            (KeyCode::Char('d'), KeyModifiers::NONE) | (KeyCode::Delete, _) => Act::Discard,
            _ => return None,
        };
        let file = self.changes.file_under_cursor()?;
        if act == Act::Discard {
            changes::confirm_discard(cx, &self.root, file);
        } else {
            self.changes.act(act, file);
        }
        Some(EventResult::Consumed(None))
    }

    pub fn contains(&self, row: u16, column: u16) -> bool {
        self.open
            && row >= self.area.y
            && row < self.area.bottom()
            && column >= self.area.x
            && column < self.area.right()
    }

    /// Answers `Consumed` only for an event that changed something: the terminal reports
    /// every motion of the pointer, and a consumed event is a whole screen drawn again.
    pub fn handle_mouse(&mut self, event: &MouseEvent, cx: &mut commands::Context) -> EventResult {
        let editor = &mut cx.editor;
        let separator = self.area.right().saturating_sub(1);
        let panes = self.stacked_areas();
        let divider = panes.map(|panes| panes[1].y);
        let pointer_selects_pane = matches!(
            event.kind,
            MouseEventKind::Down(_) | MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        );
        let mut pane_changed = false;
        if pointer_selects_pane && event.column != separator {
            if let Some(panes) = panes {
                for (index, pane) in panes.iter().enumerate() {
                    if event.row > pane.y && event.row < pane.bottom() {
                        let lower = index == 1;
                        if self.lower_focused() != lower {
                            self.focus_lower(lower);
                            pane_changed = true;
                        }
                    }
                }
            }
        }
        let on_sort_label = divider == Some(event.row)
            && self.tab == TabKind::Files
            && event.column >= self.sort_columns.0
            && event.column < self.sort_columns.1;
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if event.column == separator => {
                self.resizing = true;
            }
            // The order is written on the rule between the tree and the outline: a click
            // on the words turns it over, a press anywhere else on the rule drags it.
            MouseEventKind::Down(MouseButton::Left) if on_sort_label => {
                self.toggle_outline_sort(editor);
            }
            MouseEventKind::Down(MouseButton::Left) if divider == Some(event.row) => {
                self.resizing_split = true;
            }
            MouseEventKind::Drag(MouseButton::Left) if self.resizing_split => {
                if self.tab == TabKind::Commits {
                    self.commit_layout.resize_split(self.area, event.row);
                } else {
                    self.outline.resize_split(self.area, event.row);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.resizing => {
                let total = if self.code_hidden() {
                    self.area.width
                } else {
                    self.area.width + editor.tree.area().width
                };
                self.code_hidden = false;
                let most = total.saturating_sub(EDITOR_ROOM).max(MIN_WIDTH);
                let wanted = event.column.saturating_sub(self.area.x).saturating_add(1);
                let wanted = wanted.clamp(MIN_WIDTH, most);
                if self.tab == TabKind::Commits {
                    self.commit_layout.width = Some(wanted);
                } else {
                    self.width = Some(wanted);
                }
            }
            MouseEventKind::Up(MouseButton::Left) if self.resizing() => {
                let split = self.resizing_split;
                self.resizing = false;
                self.resizing_split = false;
                let result = if self.tab == TabKind::Commits {
                    self.commit_layout.save()
                } else if split {
                    self.outline.save()
                } else if let Some(width) = self.width {
                    panel_width::save(WIDTH_FILE, width)
                } else {
                    Ok(())
                };
                if let Err(err) = result {
                    log::error!("Could not remember the panel layout: {err:#}");
                    editor.set_error(format!("Could not remember the panel layout: {err:#}"));
                }
            }
            MouseEventKind::Down(MouseButton::Right) if self.tab == TabKind::Commits => {
                return editor::open_review_menu(event.row, event.column);
            }
            // The outline's own menu: the order, and a way to put it away.
            MouseEventKind::Down(MouseButton::Right) if self.outline.focused => {
                let pane = self.active_area();
                if event.row > pane.y {
                    let line = (event.row - pane.y) as usize;
                    if let Some(index) = self.outline.list().row_at(line - 1) {
                        self.outline.list_mut().select(index);
                    }
                }
                return open_outline_menu(event.row, event.column, self.outline.by_name());
            }
            // The right button takes the row it lands on and offers what can be done to it.
            MouseEventKind::Down(MouseButton::Right) if self.active().edits_disk() => {
                let line = event.row.saturating_sub(self.area.y) as usize;
                if line > 0 {
                    if let Some(index) = self.active().list().row_at(line - 1) {
                        self.active_mut().list_mut().select(index);
                        self.cursor_moved(editor);
                    }
                }

                let hidden = self.files.hidden(editor);
                let outline = self.outline.shown();
                return open_menu(
                    event.row,
                    event.column,
                    self.prompt_target(),
                    hidden,
                    outline,
                );
            }
            MouseEventKind::Down(MouseButton::Right) if self.tab == TabKind::Changes => {
                let line = event.row.saturating_sub(self.area.y) as usize;
                if line > 0 {
                    if let Some(index) = self.active().list().row_at(line - 1) {
                        self.active_mut().list_mut().select(index);
                    }
                }
                let Some(file) = self.changes.file_under_cursor() else {
                    return EventResult::Consumed(None);
                };

                return open_changes_menu(event.row, event.column, self.root.clone(), file);
            }
            // A click never takes the keyboard from the text: the row is shown and chosen,
            // and typing still types. The sidebar has the keys only when asked by a key of
            // its own, Ctrl-E or Ctrl-R, or when there is no code on screen to type into.
            MouseEventKind::Down(MouseButton::Left) => {
                if self.code_hidden() {
                    self.focused = true;
                }
                // The first line holds the tabs, not a row.
                let line = event.row.saturating_sub(self.area.y) as usize;
                if line == 0 {
                    let hit = TabKind::ALL
                        .into_iter()
                        .zip(self.tab_columns)
                        .find(|(_, (from, to))| event.column >= *from && event.column < *to);
                    if let Some((kind, _)) = hit {
                        self.switch_tab(kind, editor);
                    }
                    return EventResult::Consumed(None);
                }
                let pane = self.active_area();
                if event.row <= pane.y || event.row >= pane.bottom() {
                    return EventResult::Consumed(None);
                }
                let line = (event.row - pane.y) as usize;
                let Some(index) = self.active().list().row_at(line - 1) else {
                    return EventResult::Consumed(None);
                };
                let now = Instant::now();
                let double = self
                    .last_click
                    .is_some_and(|(row, at)| row == index && now.duration_since(at) < DOUBLE_CLICK);
                self.last_click = if double { None } else { Some((index, now)) };
                let before = self.active().list().cursor;
                self.active_mut().list_mut().select(index);
                if index != before {
                    self.cursor_moved(editor);
                }
                let how = if double {
                    Activation::Double
                } else {
                    Activation::Click
                };
                if self.open_row(editor, how) {
                    // What opened is where the typing goes, even when the tree had the keys.
                    self.focus_code();
                }
            }
            MouseEventKind::ScrollDown => {
                if event.row <= self.active_area().y || event.row >= self.active_area().bottom() {
                    return EventResult::Ignored(None);
                }
                let lines = editor.config().scroll_lines;
                if !self.scroll_by(editor, lines) && !pane_changed {
                    return EventResult::Ignored(None);
                }
            }
            MouseEventKind::ScrollUp => {
                if event.row <= self.active_area().y || event.row >= self.active_area().bottom() {
                    return EventResult::Ignored(None);
                }
                let lines = editor.config().scroll_lines;
                if !self.scroll_by(editor, -lines) && !pane_changed {
                    return EventResult::Ignored(None);
                }
            }
            // A motion, a release, a drag of nothing: nothing to draw again.
            _ => return EventResult::Ignored(None),
        }
        EventResult::Consumed(None)
    }

    /// Walks to the next row whose name starts with what has been typed. Letters typed one
    /// after another build a longer name; a pause starts a new one.
    fn jump_to_typed(&mut self, char: char) {
        let now = Instant::now();
        let carried = self
            .typed
            .1
            .is_some_and(|at| now.duration_since(at) < TYPING);

        if carried {
            self.typed.0.push(char);
        } else {
            self.typed.0 = char.to_string();
        }
        self.typed.1 = Some(now);

        let typed = self.typed.0.to_lowercase();
        let rows = self.active().rows();
        let from = self.active().list().cursor;

        // From the row after this one, so typing the same letter walks the ones that match.
        let found = (1..=rows.len())
            .map(|step| (from + step) % rows.len())
            .find(|index| {
                rows[*index]
                    .name()
                    .is_some_and(|name| name.to_lowercase().starts_with(&typed))
            });

        if let Some(index) = found {
            self.active_mut().list_mut().select(index);
        }
    }

    /// Scrolls the rows; says whether anything moved.
    fn scroll_by(&mut self, editor: &mut Editor, lines: isize) -> bool {
        let list = self.active_mut().list_mut();
        let before = (list.cursor, list.scroll);
        list.scroll_by(lines);
        let after = (list.cursor, list.scroll);
        if after.0 != before.0 {
            self.cursor_moved(editor);
        }
        after != before
    }

    pub fn render(&mut self, area: Rect, surface: &mut Surface, editor: &mut Editor) {
        self.area = area;
        if area.width < 2 || area.height < 2 {
            return;
        }
        let page = area.height.saturating_sub(1) as usize;
        match (self.tab, self.stacked_areas()) {
            (TabKind::Commits, Some([history, files])) => {
                self.commits
                    .set_pages((history.height - 1) as usize, (files.height - 1) as usize);
            }
            (TabKind::Commits, None) => self.commits.set_pages(page, page),
            (TabKind::Files, Some([tree, outline])) => {
                self.files.list_mut().set_page((tree.height - 1) as usize);
                self.outline
                    .list_mut()
                    .set_page((outline.height - 1) as usize);
            }
            _ => self.active_mut().list_mut().set_page(page),
        }
        if !self.built {
            self.came_on_screen(editor);
        }
        let current = doc!(editor).path().map(Path::to_path_buf);
        if current.is_some() && current != self.revealed {
            self.reveal_current(editor);
        }
        if self.outline_visible() {
            let keys_here = self.focused && self.outline.focused;
            self.outline.sync(editor, keys_here);
        }

        let theme = &editor.theme;
        let directory_style = theme.get("ui.text.directory");
        // The selected row reads as a menu's selected item does: a theme may give that item
        // only a background, one the sidebar's dimmed text barely shows on, so the menu's
        // own text colour comes along. Without focus the bold current file is the only mark.
        let selected_style = theme.get("ui.menu").patch(theme.get("ui.menu.selected"));
        let resting_style = theme
            .try_get("ui.cursorline.primary")
            .filter(|style| style.bg.is_some())
            .unwrap_or_else(|| theme.get("ui.menu"));
        let separator_style = theme.get("ui.window");
        let header_style = directory_style.add_modifier(Modifier::BOLD);
        let inactive_style = theme.get("ui.text.inactive");

        let content_width = area.width.saturating_sub(1) as usize;
        for y in area.y..area.bottom() {
            surface.set_string(area.right() - 1, y, "│", separator_style);
        }

        if let Some(filter) = self.filter().map(str::to_string) {
            // The box takes the strip's row; a click there is not a tab's while it is open.
            self.tab_columns = [(0, 0); TabKind::ALL.len()];
            let x = area.x + 1;
            let room = (area.right() - 1).saturating_sub(x) as usize;
            let (end, _) = surface.set_stringn(x, area.y, "Filter: ", room, inactive_style);
            let room = (area.right() - 1).saturating_sub(end) as usize;
            let (end, _) = surface.set_stringn(end, area.y, &filter, room, theme.get("ui.text"));
            if (end as usize) < area.right() as usize - 1 {
                surface.set_string(end, area.y, "▏", header_style);
            }
        } else {
            let labels = [
                self.files.label(),
                self.changes.label(),
                self.commits.label(),
            ];
            let mut x = area.x + 1;
            self.tab_columns = [(0, 0); TabKind::ALL.len()];
            for (index, (kind, label)) in TabKind::ALL.iter().zip(labels).enumerate() {
                if kind.needs_git() && !self.in_git {
                    continue;
                }
                let style = if *kind == self.tab {
                    header_style
                } else {
                    inactive_style
                };
                let room = (area.right() - 1).saturating_sub(x) as usize;
                let (end, _) = surface.set_stringn(x, area.y, &label, room, style);
                self.tab_columns[index] = (x, end);
                x = end + 2;
            }
        }

        self.sort_columns = (0, 0);
        let mut sort_columns = (0, 0);
        let tab = self.active();
        if tab.rows().is_empty() && self.stacked_areas().is_none() {
            if let Some(message) = tab.empty_message() {
                let style = if message.is_error {
                    theme.get("error")
                } else {
                    inactive_style
                };
                let width = content_width.saturating_sub(1);
                let paint = |_: usize| style;
                surface.set_string_truncated(
                    area.x + 1,
                    area.y + 1,
                    &message.text,
                    width,
                    paint,
                    true,
                    false,
                );
            }
            return;
        }

        let stacked = self.stacked_areas();
        let split = stacked.is_some();
        let panes = if let Some([upper_area, lower_area]) = stacked {
            for x in area.x..area.right().saturating_sub(1) {
                surface.set_string(x, lower_area.y, "─", separator_style);
            }
            surface.set_string(area.right() - 1, lower_area.y, "┤", separator_style);
            let heading = if self.tab == TabKind::Commits {
                " Files "
            } else {
                " Outline "
            };
            let (end, _) = surface.set_stringn(
                area.x + 1,
                lower_area.y,
                heading,
                content_width.saturating_sub(1),
                header_style,
            );
            if self.tab == TabKind::Files {
                // The order, at the right edge of the rule, where a click turns it over.
                let label = format!(" {} ", self.outline.sort_label());
                let right = area.right().saturating_sub(2);
                let x = right.saturating_sub(label.len() as u16);
                if x > end {
                    let style = if self.outline.focused {
                        header_style
                    } else {
                        inactive_style
                    };
                    let (label_end, _) =
                        surface.set_stringn(x, lower_area.y, &label, (right - x) as usize, style);
                    sort_columns = (x, label_end);
                }
            }
            match self.tab {
                TabKind::Commits => {
                    let [(history, history_list, history_focus), (files, files_list, files_focus)] =
                        self.commits.panes();
                    vec![
                        (history, history_list, history_focus, upper_area, false),
                        (files, files_list, files_focus, lower_area, false),
                    ]
                }
                _ => vec![
                    (
                        self.files.rows(),
                        self.files.list(),
                        !self.outline.focused,
                        upper_area,
                        false,
                    ),
                    (
                        self.outline.rows(),
                        self.outline.list(),
                        self.outline.focused,
                        lower_area,
                        true,
                    ),
                ],
            }
        } else {
            vec![(tab.rows(), tab.list(), true, area, false)]
        };
        for (rows, list, focused, area, is_outline) in panes {
            if is_outline && rows.is_empty() {
                if let Some(message) = self.outline.empty_message() {
                    let width = content_width.saturating_sub(1);
                    surface.set_string_truncated(
                        area.x + 1,
                        area.y + 1,
                        &message.text,
                        width,
                        |_| inactive_style,
                        true,
                        false,
                    );
                }
                continue;
            }
            let rows = rows.iter().enumerate().skip(list.scroll).take(list.page);
            for (index, row) in rows {
                let y = area.y + 1 + (index - list.scroll) as u16;
                let line = Rect::new(area.x, y, area.width.saturating_sub(1), 1);
                // Without the focus the row keeps a quieter mark, so what was chosen stays
                // in sight while it is read on the right.
                let selected = (index == list.cursor).then_some(if self.focused && focused {
                    selected_style
                } else if split {
                    theme.get("ui.text.inactive").add_modifier(Modifier::BOLD)
                } else {
                    resting_style
                });
                if let Some(selected) = selected {
                    surface.set_style(line, selected);
                }
                let current = match row {
                    Row::Symbol(_) => self.outline.is_current(index),
                    _ => current
                        .as_deref()
                        .is_some_and(|current| row.path() == Some(current)),
                };
                let paint = RowPaint {
                    line,
                    selected,
                    current,
                };
                match row {
                    Row::Entry(entry) => {
                        let folds = self.tab_view().folds();
                        let open = folds.is_some_and(|folds| folds.is_open(&entry.path));
                        entries::draw_entry(surface, &paint, entry, open, theme);
                    }
                    Row::Commit(commit) => commits::draw_commit(surface, &paint, commit, theme),
                    Row::Symbol(symbol) => outline::draw_symbol(surface, &paint, symbol, theme),
                }
            }
        }
        self.sort_columns = sort_columns;
    }
}

/// Runs `work` off the main thread and hands what it made to the sidebar, on it.
pub(crate) fn background<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
    land: impl FnOnce(&mut Sidebar, &mut Editor, T) + Send + 'static,
) {
    editor::background(work, move |editor, view, answer| {
        land(&mut view.sidebar, editor, answer);
    });
}

/// Calls `then` on the sidebar after `delay`, for what asks git again while on screen.
pub(crate) fn later(
    delay: Duration,
    then: impl FnOnce(&mut Sidebar, &mut Editor) + Send + 'static,
) {
    editor::later(delay, move |editor, view| then(&mut view.sidebar, editor));
}

/// What can be done to the row the pointer is on. Every one of them has a key as well —
/// the menu is the other way in, never the only one.
fn open_menu(
    row: u16,
    column: u16,
    target: PromptTarget,
    hidden: bool,
    outline: bool,
) -> EventResult {
    EventResult::Consumed(Some(Box::new(move |compositor, _cx| {
        let for_new = target.clone();
        let for_rename = target.clone();
        let for_delete = target.clone();
        let to_copy = target.path.clone();
        let to_reveal = target;

        let mut entries = vec![
            context_menu::Entry::new(
                "New file or folder",
                "Ctrl-Alt-n",
                Box::new(move |compositor, cx| {
                    context_menu::with_context(compositor, cx, |cx| files::prompt_new(cx, for_new))
                }),
            ),
            context_menu::Entry::new(
                "Rename",
                "Ctrl-Alt-r",
                Box::new(move |compositor, cx| {
                    context_menu::with_context(compositor, cx, |cx| {
                        files::prompt_rename(cx, for_rename)
                    })
                }),
            ),
            context_menu::Entry::new(
                "Delete",
                "Del",
                Box::new(move |compositor, cx| {
                    context_menu::with_context(compositor, cx, |cx| {
                        files::prompt_delete(cx, for_delete)
                    })
                }),
            ),
            context_menu::Entry::new(
                if hidden {
                    "Show hidden files"
                } else {
                    "Hide hidden files"
                },
                ".",
                Box::new(|compositor, cx| {
                    if let Some(view) = compositor.find::<editor::EditorView>() {
                        view.sidebar.toggle_hidden(cx.editor);
                    }
                }),
            ),
            context_menu::Entry::new(
                "Copy the path",
                "",
                Box::new(move |_compositor, cx| {
                    let Some(path) = to_copy else {
                        return;
                    };

                    let path = path.to_string_lossy().into_owned();
                    if let Err(err) = cx.editor.registers.write('+', vec![path]) {
                        cx.editor.set_error(err.to_string());
                    }
                }),
            ),
        ];
        // Only where there is a desktop to open it on: over SSH it would open nowhere.
        if files::has_desktop() {
            entries.push(context_menu::Entry::new(
                files::reveal_label(),
                "Alt-Shift-r",
                Box::new(move |_compositor, cx| {
                    let path = to_reveal.path.unwrap_or(to_reveal.root);
                    files::reveal(cx.editor, path);
                }),
            ));
        }
        entries.extend([context_menu::Entry::new(
            if outline {
                "Hide the outline"
            } else {
                "Show the outline"
            },
            "Ctrl-Alt-o",
            Box::new(|compositor, cx| {
                if let Some(view) = compositor.find::<editor::EditorView>() {
                    view.sidebar.toggle_outline(cx.editor);
                }
            }),
        )]);

        compositor.push(Box::new(context_menu::ContextMenu::new(
            (row, column),
            entries,
        )));
    })))
}

/// What can be done to the outline from a row of it: the order, and putting it away.
fn open_outline_menu(row: u16, column: u16, by_name: bool) -> EventResult {
    EventResult::Consumed(Some(Box::new(move |compositor, _cx| {
        let entries = vec![
            context_menu::Entry::new(
                "Go to the definition",
                "Enter",
                Box::new(|compositor, cx| {
                    let Some(view) = compositor.find::<editor::EditorView>() else {
                        return;
                    };
                    if view.sidebar.open_row(cx.editor, Activation::Enter) {
                        view.sidebar.focus_code();
                    }
                }),
            ),
            context_menu::Entry::new(
                if by_name {
                    "List in the file's order"
                } else {
                    "List by name"
                },
                "",
                Box::new(|compositor, cx| {
                    if let Some(view) = compositor.find::<editor::EditorView>() {
                        view.sidebar.toggle_outline_sort(cx.editor);
                    }
                }),
            ),
            context_menu::Entry::new(
                "Hide the outline",
                "Ctrl-Alt-o",
                Box::new(|compositor, cx| {
                    if let Some(view) = compositor.find::<editor::EditorView>() {
                        view.sidebar.toggle_outline(cx.editor);
                    }
                }),
            ),
        ];

        compositor.push(Box::new(context_menu::ContextMenu::new(
            (row, column),
            entries,
        )));
    })))
}

/// What can be done to a changed file from the row the pointer is on; each has its key.
fn open_changes_menu(row: u16, column: u16, root: PathBuf, file: git::ChangedFile) -> EventResult {
    EventResult::Consumed(Some(Box::new(move |compositor, _cx| {
        let mut entries = vec![
            context_menu::Entry::new(
                "Show changes",
                "Enter",
                Box::new(move |compositor, cx| {
                    let Some(view) = compositor.find::<editor::EditorView>() else {
                        return;
                    };
                    let mut tab_cx = TabContext {
                        editor: cx.editor,
                        diff: &mut view.sidebar.diff,
                    };
                    if view.sidebar.changes.open(&mut tab_cx, Activation::Enter) == Outcome::Leave {
                        view.sidebar.focus_code();
                    }
                }),
            ),
            context_menu::Entry::new(
                "Open file",
                "o",
                Box::new(move |compositor, cx| {
                    let Some(view) = compositor.find::<editor::EditorView>() else {
                        return;
                    };
                    if view.sidebar.changes.open_file(cx.editor) {
                        view.sidebar.focus_code();
                    }
                }),
            ),
        ];
        for act in [Act::Stage, Act::Unstage, Act::Discard] {
            let file = file.clone();
            let root = root.clone();
            entries.push(context_menu::Entry::new(
                act.label(),
                act.key(),
                Box::new(move |compositor, cx| {
                    if act == Act::Discard {
                        context_menu::with_context(compositor, cx, |cx| {
                            changes::confirm_discard(cx, &root, file)
                        });
                        return;
                    }
                    if let Some(view) = compositor.find::<editor::EditorView>() {
                        view.sidebar.changes.act(act, file);
                    }
                }),
            ));
        }

        compositor.push(Box::new(context_menu::ContextMenu::new(
            (row, column),
            entries,
        )));
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> KeyEvent {
        name.parse().expect("a key of ours")
    }

    #[test]
    fn the_editors_shortcuts_pass_through_the_sidebar() {
        assert!(is_editor_shortcut(key("C-q")));
        assert!(is_editor_shortcut(key("C-s")));
        assert!(is_editor_shortcut(key("A-z")));
        assert!(is_editor_shortcut(key("Cmd-s")));
        assert!(is_editor_shortcut(key("F12")));

        // What the sidebar reads as its own: plain keys, which would otherwise run a
        // command on the file behind it.
        assert!(!is_editor_shortcut(key("j")));
        assert!(!is_editor_shortcut(key("i")));
        assert!(!is_editor_shortcut(key("ret")));
        assert!(!is_editor_shortcut(key("space")));
    }
}
