//! The Files tab: the workspace as it is on disk, following the file being edited, with
//! the prompts that create, rename and delete.
//!
//! The disk is never read while drawing. Each directory's listing is read off the main
//! thread and kept; the rows are laid out from what is kept, and a directory that is
//! open but not read yet is asked for, its row waiting childless until the answer lands.
//! While the tab is on screen the open directories are looked at every few seconds, and
//! the ones whose modification time moved are read again.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use helix_view::editor::{Action, FileExplorerConfig};
use helix_view::Editor;

use super::entries::{self, Folds, Listed, Listings, Row};
use super::list::List;
use super::tab::{Activation, Outcome, TabContext, TabView};
use super::{TabKind, REFRESH};
use crate::commands;
use crate::compositor;
use crate::job;
use crate::ui;
use crate::ui::confirm::{Answer, Confirm};
use crate::ui::EditorView;

pub struct FilesTab {
    root: PathBuf,
    folds: Folds,
    rows: Vec<Row>,
    list: List,
    listings: Listings,
    /// The directories being read right now.
    asking: HashSet<PathBuf>,
    /// The path to move onto once the directories down to it have landed.
    pending: Option<Pending>,
    /// Whether the next look at the disk is already on its way.
    armed: bool,
    /// Whether a look at the disk is running.
    polling: bool,
    /// What the rows are narrowed to while the filter box is open.
    filter: Option<String>,
    /// The whole workspace, read once when the box opened, for the filter to look
    /// through; until it lands the filter looks through the rows listed so far.
    walk: Option<entries::Walk>,
    /// The folds as they were when the box opened, put back when it closes.
    folds_before: Option<Folds>,
    /// Whether files that start with a dot are left out: the configuration's, taken when
    /// the tree first reads the disk and changed with it.
    hidden: Option<bool>,
    /// Counts the times `hidden` changed, so a read made the other way is dropped.
    epoch: u32,
}

struct Pending {
    path: PathBuf,
    /// Whether the row goes mid-screen, as a file just switched to does; one just made or
    /// renamed stays where the tree already is.
    center: bool,
}

impl FilesTab {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            folds: Folds::closed(),
            rows: Vec::new(),
            list: List::default(),
            listings: Listings::new(),
            asking: HashSet::new(),
            pending: None,
            armed: false,
            polling: false,
            filter: None,
            walk: None,
            folds_before: None,
            hidden: None,
            epoch: 0,
        }
    }

    /// Whether files that start with a dot are left out of the tree.
    pub fn hidden(&mut self, editor: &Editor) -> bool {
        *self
            .hidden
            .get_or_insert(editor.config().file_explorer.hidden)
    }

    /// Leaves files that start with a dot out of the tree, or brings them back: what is
    /// on screen is read again, and the rows stay as they are until it lands.
    pub fn set_hidden(&mut self, editor: &mut Editor, hidden: bool) {
        if self.hidden(editor) == hidden {
            return;
        }
        self.hidden = Some(hidden);
        self.epoch = self.epoch.wrapping_add(1);
        self.asking.clear();
        if self.filter.is_some() {
            self.walk_workspace(editor);
        }
        let on_screen = self.dirs_on_screen();
        self.listings.retain(|dir, _| on_screen.contains(dir));
        self.ask(editor, on_screen);
    }

    /// The explorer's configuration, with the tree's own word on files that start with a dot.
    fn explorer_config(&mut self, editor: &Editor) -> FileExplorerConfig {
        let mut config = editor.config().file_explorer.clone();
        config.hidden = self.hidden(editor);
        config
    }

    /// Reads the whole workspace for the filter box, off the main thread.
    fn walk_workspace(&mut self, editor: &Editor) {
        let root = self.root.clone();
        let config = self.explorer_config(editor);
        let epoch = self.epoch;
        super::background(
            move || entries::walk_workspace(&root, &config),
            move |sidebar, editor, walk| {
                if sidebar.files.epoch == epoch {
                    sidebar.files.walk_landed(editor, walk);
                }
            },
        );
    }

    pub fn filter(&self) -> Option<&str> {
        self.filter.as_deref()
    }

    /// Opens the filter box, or narrows the rows to `text` while it is open; `None`
    /// closes it and the whole tree comes back, folded as it was.
    pub fn set_filter(&mut self, editor: &mut Editor, text: Option<String>) {
        let opening = text.is_some() && self.filter.is_none();
        let closing = text.is_none() && self.filter.is_some();
        if opening {
            self.folds_before = Some(self.folds.clone());
            self.walk_workspace(editor);
        }
        if closing {
            if let Some(folds) = self.folds_before.take() {
                self.folds = folds;
            }
            self.walk = None;
        }
        self.filter = text;
        self.rebuild(editor);
    }

    /// The walk of the workspace landed; kept only while the box that asked is still open.
    fn walk_landed(&mut self, editor: &mut Editor, walk: entries::Walk) {
        if self.filter.is_none() {
            return;
        }
        if walk.capped {
            editor.set_status(format!(
                "the filter looks through the first {} files only",
                entries::WALK_CAP
            ));
        }
        self.walk = Some(walk);
        self.rebuild(editor);
    }

    /// Something on disk changed under `path`, by one of the sidebar's own prompts: the
    /// directory holding it is read again, and the cursor lands on it when it is there.
    pub fn disk_changed(&mut self, editor: &mut Editor, path: &Path) {
        self.folds.open_ancestors(&self.root, path);
        let dir = self.listed_dir_holding(path);
        self.pending = Some(Pending {
            path: path.to_path_buf(),
            center: false,
        });
        self.ask(editor, vec![dir]);
        self.rebuild(editor);
    }

    /// The nearest directory with a listing that shows `path`: itself when it is a listed
    /// directory whose contents changed, else the first listed one above it, since a
    /// flattened chain of directories is listed at its end alone.
    fn listed_dir_holding(&self, path: &Path) -> PathBuf {
        let mut dir = Some(path);
        while let Some(current) = dir {
            if current == self.root || self.listings.contains_key(current) {
                return current.to_path_buf();
            }
            dir = current.parent();
        }
        self.root.clone()
    }

    /// Reads `dirs` off the main thread, the ones already being read left out.
    fn ask(&mut self, editor: &mut Editor, dirs: Vec<PathBuf>) {
        let dirs: Vec<PathBuf> = dirs
            .into_iter()
            .filter(|dir| self.asking.insert(dir.clone()))
            .collect();
        if dirs.is_empty() {
            return;
        }
        let config = self.explorer_config(editor);
        let epoch = self.epoch;
        super::background(
            move || {
                dirs.iter()
                    .map(|dir| entries::list_dir(dir, &config))
                    .collect::<Vec<Listed>>()
            },
            move |sidebar, editor, listed| {
                if sidebar.files.epoch == epoch {
                    sidebar.files.landed(editor, listed);
                }
            },
        );
    }

    fn landed(&mut self, editor: &mut Editor, listed: Vec<Listed>) {
        for read in listed {
            self.asking.remove(&read.dir);
            if let Some(error) = read.error {
                editor.set_error(error);
            }
            self.listings.insert(read.dir, read.listing);
        }
        self.rebuild(editor);
        self.settle_pending();
    }

    /// Moves onto the path waited for once it is among the rows; a path that never shows
    /// up, because nothing more is being read, stops being waited for.
    fn settle_pending(&mut self) {
        let Some(pending) = &self.pending else {
            return;
        };
        let found = self
            .rows
            .iter()
            .position(|row| row.path() == Some(pending.path.as_path()));
        match found {
            Some(index) => {
                self.list.select(index);
                if pending.center {
                    self.list.center();
                }
                self.pending = None;
            }
            None if self.asking.is_empty() => self.pending = None,
            None => {}
        }
    }

    /// The directories the rows are laid out from: the root and every open one read so far.
    fn dirs_on_screen(&self) -> Vec<PathBuf> {
        let mut dirs = vec![self.root.clone()];
        let open = self
            .listings
            .keys()
            .filter(|dir| **dir != self.root && self.folds.is_open(dir))
            .cloned();
        dirs.extend(open);
        dirs
    }

    /// Starts the looks at the disk, one every `REFRESH` while the tab is on screen.
    fn watch(&mut self) {
        if self.armed {
            return;
        }
        self.armed = true;
        super::later(REFRESH, |sidebar, editor| {
            sidebar.files.armed = false;
            if sidebar.showing(TabKind::Files) {
                sidebar.files.poll(editor);
            }
        });
    }

    /// Looks at the open directories off the main thread and reads again the ones that
    /// moved; a read already under way is left to land first.
    fn poll(&mut self, editor: &mut Editor) {
        // The configuration changed some other way, by hand or on the settings screen.
        let configured = editor.config().file_explorer.hidden;
        if self.hidden.is_some_and(|hidden| hidden != configured) {
            self.set_hidden(editor, configured);
        }
        if self.polling || !self.asking.is_empty() {
            self.watch();
            return;
        }
        self.polling = true;
        let config = self.explorer_config(editor);
        let epoch = self.epoch;
        let watched: Vec<(PathBuf, entries::Listing)> = self
            .dirs_on_screen()
            .into_iter()
            .filter_map(|dir| {
                let listing = self.listings.get(&dir)?.clone();
                Some((dir, listing))
            })
            .collect();
        super::background(
            move || {
                watched
                    .iter()
                    .filter(|(_, listing)| entries::fingerprint_moved(listing))
                    .map(|(dir, _)| entries::list_dir(dir, &config))
                    .collect::<Vec<Listed>>()
            },
            move |sidebar, editor, listed| {
                sidebar.files.polling = false;
                if !listed.is_empty() && sidebar.files.epoch == epoch {
                    sidebar.files.landed(editor, listed);
                }
                if sidebar.showing(TabKind::Files) {
                    sidebar.files.watch();
                }
            },
        );
    }
}

impl TabView for FilesTab {
    fn label(&self) -> String {
        "Files".to_string()
    }

    fn rows(&self) -> &[Row] {
        &self.rows
    }

    fn list(&self) -> &List {
        &self.list
    }

    fn list_mut(&mut self) -> &mut List {
        &mut self.list
    }

    fn folds(&self) -> Option<&Folds> {
        Some(&self.folds)
    }

    fn folds_mut(&mut self) -> Option<&mut Folds> {
        Some(&mut self.folds)
    }

    /// Lays the rows out from the listings held, asking for the open directories that
    /// have none; until those land the rows stay as they are, never blank.
    fn rebuild(&mut self, editor: &mut Editor) {
        let selected = self
            .rows
            .get(self.list.cursor)
            .and_then(Row::path)
            .map(Path::to_path_buf);
        let mut rows = Vec::new();
        let mut missing = Vec::new();
        entries::list_cached(
            &self.root,
            0,
            &self.folds,
            &self.listings,
            &mut rows,
            &mut missing,
        );
        let unread_root = !self.listings.contains_key(&self.root);
        let filter = self.filter.as_deref().filter(|text| !text.is_empty());
        let mut laid_out = !unread_root;
        if let Some(filter) = filter {
            if let Some(walk) = &self.walk {
                rows = entries::narrow_walk(&self.root, &walk.files, filter);
                laid_out = true;
            } else {
                rows = entries::narrow(rows, &self.root, filter);
            }
        }
        if laid_out {
            self.rows = rows;
            entries::reselect(&self.rows, &mut self.list, selected.as_deref());
            // A filter is typed to open a file: the cursor waits on the first one that
            // matches, so Enter opens it and never folds the directory above it.
            if filter.is_some() {
                let first_file = self
                    .rows
                    .iter()
                    .position(|row| row.entry().is_some_and(|entry| !entry.is_dir));
                if let Some(index) = first_file {
                    self.list.select(index);
                }
            }
        }
        self.ask(editor, missing);
    }

    fn shown(&mut self, _cx: &mut TabContext) {
        self.watch();
    }

    /// F5: every directory on screen is read again, and what is folded away is forgotten,
    /// so it is read fresh when opened.
    fn refresh(&mut self, cx: &mut TabContext) {
        let on_screen = self.dirs_on_screen();
        self.listings.retain(|dir, _| on_screen.contains(dir));
        self.ask(cx.editor, on_screen);
    }

    fn open(&mut self, cx: &mut TabContext, _how: Activation) -> Outcome {
        let Some(path) = self.rows.get(self.list.cursor).and_then(Row::path) else {
            return Outcome::Stay;
        };
        if let Err(err) = cx.editor.open(path, Action::Replace) {
            cx.editor
                .set_error(format!("unable to open \"{}\": {}", path.display(), err));
            return Outcome::Stay;
        }
        Outcome::Leave
    }

    fn reveal(&mut self, editor: &mut Editor, path: &Path) {
        if !path.starts_with(&self.root) {
            return;
        }
        self.folds.open_ancestors(&self.root, path);
        self.pending = Some(Pending {
            path: path.to_path_buf(),
            center: true,
        });
        self.rebuild(editor);
        self.settle_pending();
    }

    fn edits_disk(&self) -> bool {
        true
    }
}

/// Where a prompt acts: the workspace, and the entry under the cursor when it needs one.
#[derive(Clone)]
pub struct PromptTarget {
    pub root: PathBuf,
    pub path: Option<PathBuf>,
    pub is_dir: bool,
}

impl PromptTarget {
    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    /// The directory a new entry goes in: the one under the cursor, else the cursor's
    /// parent, else the root.
    fn dir(&self) -> PathBuf {
        let Some(path) = &self.path else {
            return self.root.clone();
        };
        if self.is_dir {
            return path.clone();
        }
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.root.clone())
    }
}

/// `Ctrl-Alt-n`: a file, or a directory when the name ends in `/`, next to the cursor.
pub fn prompt_new(cx: &mut commands::Context, target: PromptTarget) {
    cx.push_layer(new_dialog(target));
}

/// The prompt that makes an entry next to `target`, for whoever pushes it.
pub fn new_dialog(target: PromptTarget) -> Box<dyn compositor::Component> {
    let dir = target.dir();
    let mut line = target.relative(&dir);
    if !line.is_empty() {
        line.push('/');
    }
    let root = target.root;
    let ask = ui::ask::Ask::new(
        "New file or folder",
        "A name ending in / makes a folder",
        "Create",
        Box::new(move |cx: &mut compositor::Context, input: String| {
            let made = match inside(&root, input.trim()) {
                Ok(made) => made,
                Err(err) => {
                    cx.editor.set_error(err);
                    return;
                }
            };
            if made.exists() {
                cx.editor
                    .set_error(format!("{} already exists", made.display()));
                return;
            }
            let result = if input.trim_end().ends_with('/') {
                std::fs::create_dir_all(&made)
            } else {
                create_file(&made)
            };
            if let Err(err) = result {
                cx.editor.set_error(format!("{}: {}", made.display(), err));
                return;
            }
            // A new file is made to be written in: it opens and the keys go to it, rather
            // than staying in the tree where they would walk the rows instead of typing.
            let mut to_code = false;
            if !made.is_dir() {
                match cx.editor.open(&made, Action::Replace) {
                    Ok(_) => to_code = true,
                    Err(err) => cx.editor.set_error(format!("{}: {}", made.display(), err)),
                }
            }
            disk_changed_then(cx, made, to_code);
        }),
    )
    .with_line(&line);

    Box::new(ask)
}

/// `Ctrl-Alt-r`: the entry under the cursor gets a new name, its open buffers going along.
pub fn prompt_rename(cx: &mut commands::Context, target: PromptTarget) {
    if let Some(dialog) = rename_dialog(target) {
        cx.push_layer(dialog);
    }
}

/// The prompt that renames `target`, for whoever pushes it.
pub fn rename_dialog(target: PromptTarget) -> Option<Box<dyn compositor::Component>> {
    let source = target.path.clone()?;
    let line = target.relative(&source);
    let root = target.root;
    let ask = ui::ask::Ask::new(
        "Rename",
        "The new name, or a path to move it to",
        "Rename",
        Box::new(move |cx: &mut compositor::Context, input: String| {
            let renamed_to = match inside(&root, input.trim().trim_end_matches('/')) {
                Ok(renamed_to) => renamed_to,
                Err(err) => {
                    cx.editor.set_error(err);
                    return;
                }
            };
            if renamed_to == source {
                return;
            }
            if renamed_to.exists() {
                cx.editor
                    .set_error(format!("{} already exists", renamed_to.display()));
                return;
            }
            let parent_made = renamed_to
                .parent()
                .map(std::fs::create_dir_all)
                .unwrap_or(Ok(()));
            let renamed = parent_made.and_then(|_| std::fs::rename(&source, &renamed_to));
            if let Err(err) = renamed {
                cx.editor
                    .set_error(format!("{}: {}", source.display(), err));
                return;
            }
            retarget_documents(cx.editor, &source, &renamed_to);
            // Both ends changed: where it was is read again first, where it went last, so
            // the cursor follows it there.
            if let Some(from) = source.parent() {
                disk_changed(cx, from.to_path_buf());
            }
            disk_changed(cx, renamed_to);
        }),
    )
    .with_line(&line);

    Some(Box::new(ask))
}

/// `Del`: the entry under the cursor is deleted after a confirmation, its buffers closed.
pub fn prompt_delete(cx: &mut commands::Context, target: PromptTarget) {
    if let Some(dialog) = delete_dialog(cx.editor, target) {
        cx.push_layer(dialog);
    }
}

/// The confirmation that deletes `target`, for whoever pushes it: the sidebar's own key,
/// the context menu, or the shortcut that reaches the tree from anywhere.
pub fn delete_dialog(
    editor: &Editor,
    target: PromptTarget,
) -> Option<Box<dyn compositor::Component>> {
    let path = target.path.clone()?;
    let is_dir = target.is_dir;
    let kind = if is_dir { "folder" } else { "file" };
    let mut lines = vec![format!("Delete {kind} \"{}\"?", target.relative(&path))];
    if is_dir {
        lines.push("All its contents will also be deleted.".to_string());
    }
    if unsaved_under(editor, &path) {
        lines.push("Unsaved changes to it will be lost.".to_string());
    }
    let answers = vec![
        Answer::new("Cancel", Box::new(|_| {})),
        Answer::new(
            "Delete",
            Box::new(move |cx| {
                let removed = if is_dir {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                if let Err(err) = removed {
                    cx.editor.set_error(format!("{}: {}", path.display(), err));
                    return;
                }
                close_documents_under(cx.editor, &path);
                let parent = path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| path.clone());
                disk_changed(cx, parent);
            }),
        )
        .destructive(),
    ];
    Some(Box::new(Confirm::new("Confirm deletion", lines, answers)))
}

/// Whether there is a desktop to show a folder on. A Mac or Windows reached over SSH has
/// one, but not on the screen of whoever is typing; elsewhere a desktop is a display.
pub fn has_desktop() -> bool {
    let set = |name: &str| std::env::var_os(name).is_some_and(|value| !value.is_empty());
    if cfg!(any(target_os = "macos", windows)) {
        return !set("SSH_CONNECTION") && !set("SSH_TTY");
    }
    set("WAYLAND_DISPLAY") || set("DISPLAY")
}

/// What revealing is called where sid runs, as each system's own editors call it.
pub fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Reveal in Finder"
    } else if cfg!(windows) {
        "Reveal in File Explorer"
    } else {
        "Open in the file manager"
    }
}

/// Shows `path` in the system's file manager, selected in its folder where the system
/// can do that. The file manager is started off the main thread; what went wrong lands
/// in the status line.
pub fn reveal(editor: &mut Editor, path: PathBuf) {
    if !has_desktop() {
        editor.set_error("There is no desktop here to open a file manager on");
        return;
    }
    ui::editor::background(
        move || open_file_manager(&path),
        |editor, _view, opened: Result<(), String>| {
            if let Err(err) = opened {
                editor.set_error(err);
            }
        },
    );
}

/// Runs a program with nothing of the terminal's, so what it prints never lands on the
/// screen sid draws, and tells whether it went well.
#[cfg(not(windows))]
fn quietly(program: &str, args: &[&std::ffi::OsStr]) -> Result<bool, String> {
    std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .map_err(|err| format!("{program}: {err}"))
}

#[cfg(target_os = "macos")]
fn open_file_manager(path: &Path) -> Result<(), String> {
    match quietly("open", &["-R".as_ref(), path.as_os_str()])? {
        true => Ok(()),
        false => Err(format!("Finder could not show {}", path.display())),
    }
}

#[cfg(windows)]
fn open_file_manager(path: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    // Explorer reads its own command line, quotes and all, and answers failure even when
    // it opened: it is started, and not asked how it went.
    std::process::Command::new("explorer")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("explorer: {err}"))
}

/// The desktop's file manager selects the entry when it answers the freedesktop call for
/// it; when none does, the folder is opened with whatever opens folders.
#[cfg(not(any(target_os = "macos", windows)))]
fn open_file_manager(path: &Path) -> Result<(), String> {
    let item = format!("array:string:{}", file_uri(path));
    let shown = quietly(
        "dbus-send",
        &[
            "--session".as_ref(),
            "--print-reply".as_ref(),
            "--reply-timeout=5000".as_ref(),
            "--dest=org.freedesktop.FileManager1".as_ref(),
            "/org/freedesktop/FileManager1".as_ref(),
            "org.freedesktop.FileManager1.ShowItems".as_ref(),
            item.as_ref(),
            "string:".as_ref(),
        ],
    );
    if shown == Ok(true) {
        return Ok(());
    }

    let folder = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    match quietly("xdg-open", &[folder.as_os_str()])? {
        true => Ok(()),
        false => Err(format!("No file manager could open {}", folder.display())),
    }
}

/// `path` as a file:// URI, every byte that is not plainly safe in one escaped.
#[cfg(not(any(target_os = "macos", windows)))]
fn file_uri(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut uri = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-_.~".contains(&byte) {
            uri.push(byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

/// Where `name`, as typed in a prompt, lands below `root`. A name may go into
/// subdirectories, made on the way; it may not leave the project, by being absolute or by
/// climbing with `..`.
fn inside(root: &Path, name: &str) -> Result<PathBuf, String> {
    let relative = Path::new(name);
    let climbs = relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir));
    if name.is_empty() || relative.is_absolute() || climbs {
        return Err("the name must stay inside the project".to_string());
    }
    Ok(root.join(relative))
}

fn create_file(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map(|_| ())
}

/// Points every open buffer under `source` at its new location after a rename.
fn retarget_documents(editor: &mut Editor, source: &Path, target: &Path) {
    let moved: Vec<(helix_view::DocumentId, PathBuf)> = editor
        .documents()
        .filter_map(|doc| {
            let path = doc.path()?;
            let rest = path.strip_prefix(source).ok()?;
            Some((doc.id(), target.join(rest)))
        })
        .collect();
    for (id, path) in moved {
        if let Some(doc) = editor.documents.get_mut(&id) {
            doc.set_path(Some(&path));
        }
    }
}

/// Closes the buffers of files that were just deleted, whether or not they had unsaved
/// changes: a buffer left open for a file that no longer exists writes it back the next
/// time anything saves, and the file the user just deleted comes back. The confirmation
/// says so before the deletion happens.
pub(super) fn close_documents_under(editor: &mut Editor, target: &Path) {
    let gone: Vec<helix_view::DocumentId> = editor
        .documents()
        .filter(|doc| doc.path().is_some_and(|path| path.starts_with(target)))
        .map(|doc| doc.id())
        .collect();
    for id in gone {
        match editor.close_document(id, true) {
            Ok(()) | Err(helix_view::editor::CloseError::DoesNotExist) => {}
            Err(helix_view::editor::CloseError::BufferModified(name)) => {
                editor.set_error(format!("{} is modified and stays open", name));
            }
            Err(helix_view::editor::CloseError::SaveError(err)) => {
                editor.set_error(err.to_string());
            }
        }
    }
}

/// Whether anything open under `path` has changes that deleting it would throw away.
fn unsaved_under(editor: &Editor, path: &Path) -> bool {
    editor
        .documents()
        .any(|doc| doc.is_modified() && doc.path().is_some_and(|open| open.starts_with(path)))
}

/// Tells the sidebar the disk changed under `path`, once the prompt's own turn is over.
fn disk_changed(cx: &mut crate::compositor::Context, path: PathBuf) {
    disk_changed_then(cx, path, false);
}

/// The same, and with `to_code` the keys go to the file that was just opened instead of
/// staying in the tree.
fn disk_changed_then(cx: &mut crate::compositor::Context, path: PathBuf, to_code: bool) {
    let callback = Box::pin(async move {
        let call = job::Callback::EditorCompositor(Box::new(move |editor, compositor| {
            if let Some(view) = compositor.find::<EditorView>() {
                view.sidebar.disk_changed(editor, &path);
                if to_code {
                    view.sidebar.focus_code();
                }
            }
        }));
        Ok(call)
    });
    cx.jobs.callback(callback);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_stays_inside_the_project() {
        let root = Path::new("/p");
        assert_eq!(inside(root, "a.ts").unwrap(), root.join("a.ts"));
        assert_eq!(inside(root, "a/b/c.ts").unwrap(), root.join("a/b/c.ts"));
        assert_eq!(inside(root, "./a.ts").unwrap(), root.join("./a.ts"));

        assert!(inside(root, "/etc/passwd").is_err());
        assert!(inside(root, "../a.ts").is_err());
        assert!(inside(root, "a/../../b.ts").is_err());
        assert!(inside(root, "").is_err());
    }

    #[test]
    fn a_file_is_not_created_over_one_that_exists() {
        let dir = std::env::temp_dir().join(format!("sid-files-{}", std::process::id()));
        let path = dir.join("made/a.txt");
        create_file(&path).unwrap();
        assert!(path.is_file());
        let again = create_file(&path).unwrap_err();
        assert_eq!(again.kind(), std::io::ErrorKind::AlreadyExists);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
