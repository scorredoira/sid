//! The Changes tab: what `git status` names, asked again every few seconds while the tab is
//! on screen, and what can be done to a file there: staged, unstaged, discarded.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use helix_view::editor::Action;
use helix_view::Editor;

use super::diff_view::{DiffSource, DiffTarget};
use super::entries::{self, Folds, Mark, Row};
use super::git::{self, Change, ChangedFile};
use super::list::List;
use super::tab::{Activation, Message, Outcome, TabContext, TabView};
use super::{TabKind, REFRESH};
use crate::commands;
use crate::job;
use crate::ui::confirm::{Answer, Confirm};
use crate::ui::EditorView;

pub struct ChangesTab {
    root: PathBuf,
    folds: Folds,
    rows: Vec<Row>,
    list: List,
    /// What the last `git status` answered, or why it could not.
    answer: Option<git::Answer<Vec<ChangedFile>>>,
    /// The same answer by path, for the tree to mark its rows with, and the directories
    /// below the root that hold a changed file.
    by_path: HashMap<PathBuf, usize>,
    changed_dirs: HashSet<PathBuf>,
    asking: bool,
    /// Whether an ask came while one was under way, to be asked when that one lands.
    again: bool,
    /// Whether the next ask is already on its way, so coming on screen does not start a
    /// second chain of them.
    armed: bool,
    /// The file the list was last put mid-screen for, so a later answer does not move the
    /// list from where the reader scrolled it to.
    centered: Option<PathBuf>,
    /// Counts cursor moves, so only the move the cursor rested on shows its diff.
    moves: usize,
}

/// How long the cursor rests on a file before its diff is asked for.
const PREVIEW_DELAY: std::time::Duration = std::time::Duration::from_millis(150);

/// What git was asked to do to a file, so the answer says what failed and what to do next.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Act {
    Stage,
    Unstage,
    Discard,
}

impl Act {
    pub fn label(self) -> &'static str {
        match self {
            Act::Stage => "Stage",
            Act::Unstage => "Unstage",
            Act::Discard => "Discard the changes",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Act::Stage => "s",
            Act::Unstage => "u",
            Act::Discard => "d",
        }
    }
}

impl ChangesTab {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            folds: Folds::opened(),
            rows: Vec::new(),
            list: List::default(),
            answer: None,
            by_path: HashMap::new(),
            changed_dirs: HashSet::new(),
            asking: false,
            again: false,
            armed: false,
            centered: None,
            moves: 0,
        }
    }

    /// The diff of the changed file under the cursor, against the last commit.
    fn diff_target(&self) -> Option<DiffTarget> {
        let file = self.file_under_cursor()?;
        let name = file.path.file_name().map_or_else(
            || file.path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        );
        Some(DiffTarget {
            pathspecs: Vec::new(),
            name: format!("{name} (changes)"),
            source: DiffSource::WorkingTree(file),
            describe: false,
        })
    }

    /// Shows the diff of the file under the cursor, asked again: the file may have
    /// changed since it was last shown.
    fn show_diff(&self, cx: &mut TabContext) -> bool {
        let Some(target) = self.diff_target() else {
            return false;
        };
        let loader = cx.editor.syn_loader.load_full();
        cx.diff.forget();
        cx.diff.ask(target, loader);
        true
    }

    /// Opens the file under the cursor to edit it.
    pub fn open_file(&self, editor: &mut Editor) -> bool {
        let Some(entry) = self.rows.get(self.list.cursor).and_then(Row::entry) else {
            return false;
        };
        let relative = entry.path.strip_prefix(&self.root).unwrap_or(&entry.path);
        if entry.change == Some(Change::Deleted) {
            editor.set_status(format!("{} is deleted", relative.display()));
            return false;
        }
        if let Err(err) = editor.open(&entry.path, Action::Replace) {
            editor.set_error(format!(
                "unable to open \"{}\": {}",
                relative.display(),
                err
            ));
            return false;
        }
        true
    }

    /// Asks git status, off the main thread; the answer lands in `landed`. An ask while
    /// one is under way is kept, and asked when that one lands.
    pub fn ask(&mut self) {
        if self.asking {
            self.again = true;
            return;
        }
        self.asking = true;
        let root = self.root.clone();
        super::background(
            move || git::status(&root),
            |sidebar, editor, answer| {
                let shown = sidebar.wants_status();
                sidebar.changes.landed(shown, editor, answer);
            },
        );
    }

    /// Asks git status unless the next ask is already on its way: what the tree does
    /// when it comes on screen, to mark its rows.
    pub fn ask_if_idle(&mut self) {
        if !self.armed {
            self.ask();
        }
    }

    /// What the tree says of `path` beside its name: a changed file's letter, or that a
    /// directory holds changed files.
    pub fn mark(&self, path: &Path, is_dir: bool) -> Option<Mark<'_>> {
        if is_dir {
            return self.changed_dirs.contains(path).then_some(Mark::Dir);
        }
        let Some(Ok(files)) = &self.answer else {
            return None;
        };
        let index = *self.by_path.get(path)?;
        files.get(index).map(Mark::File)
    }

    fn landed(&mut self, shown: bool, editor: &mut Editor, answer: git::Answer<Vec<ChangedFile>>) {
        self.asking = false;
        let moved = self.answer.as_ref() != Some(&answer);
        self.answer = Some(answer);
        if moved {
            self.index_answer();
            let was_empty = self.rows.is_empty();
            self.rebuild(editor);
            // The file being edited goes mid-screen the first time, and when it is another
            // one; otherwise the list stays where the reader left it, the cursor on its file.
            let current = doc!(editor).path().map(Path::to_path_buf);
            if let Some(path) = current {
                if was_empty || self.centered.as_deref() != Some(path.as_path()) {
                    self.reveal(editor, &path);
                }
            }
        }
        if self.again {
            self.again = false;
            self.ask();
        }
        // For as long as the tab, or the tree it marks, is on screen, the next ask
        // follows the last answer.
        if shown && !self.armed {
            self.armed = true;
            super::later(REFRESH, |sidebar, _editor| {
                sidebar.changes.armed = false;
                if sidebar.wants_status() {
                    sidebar.changes.ask();
                }
            });
        }
    }

    /// Lays the answer out by path, and names every directory between the root and a
    /// changed file, for the tree to find its marks without a search per row.
    fn index_answer(&mut self) {
        self.by_path.clear();
        self.changed_dirs.clear();
        let Some(Ok(files)) = &self.answer else {
            return;
        };
        for (index, file) in files.iter().enumerate() {
            self.by_path.insert(file.path.clone(), index);
            let mut dir = file.path.parent();
            while let Some(current) = dir.filter(|dir| *dir != self.root) {
                if !current.starts_with(&self.root) || !self.changed_dirs.insert(current.into()) {
                    break;
                }
                dir = current.parent();
            }
        }
    }

    fn count(&self) -> usize {
        match &self.answer {
            Some(Ok(files)) => files.len(),
            _ => 0,
        }
    }

    /// The changed file under the cursor, as git last described it.
    pub fn file_under_cursor(&self) -> Option<ChangedFile> {
        let path = self.rows.get(self.list.cursor).and_then(Row::path)?;
        let Some(Ok(files)) = &self.answer else {
            return None;
        };
        files.iter().find(|file| file.path == path).cloned()
    }

    /// Does `act` to `file` off the main thread; the list is asked again when it lands, and
    /// a discarded file that is open is reloaded or closed if it was deleted.
    pub fn act(&mut self, act: Act, file: ChangedFile) {
        let root = self.root.clone();
        super::background(
            move || {
                let answer = match act {
                    Act::Stage => git::stage(&root, &file),
                    Act::Unstage => git::unstage(&root, &file),
                    Act::Discard => git::discard(&root, &file),
                };
                (act, file, answer)
            },
            |sidebar, editor, (act, file, answer)| {
                if let Err(err) = answer {
                    editor.set_error(format!("{}: {err}", act.label()));
                } else if act == Act::Discard {
                    if file.is_untracked() {
                        super::files::close_documents_under(editor, &file.path);
                    } else {
                        reload_document(editor, &file.path);
                    }
                }
                sidebar.changes.ask();
            },
        );
    }
}

/// Asks before the working changes of `file` are thrown away, in the box every question
/// is asked in. A buffer open on the file with edits not saved yet goes with it — closed
/// when the file is deleted, read again from disk when it is restored — and the box says
/// so first, as the tree's own deletion does.
pub fn confirm_discard(cx: &mut commands::Context, root: &Path, file: ChangedFile) {
    {
        let relative = file.path.strip_prefix(root).unwrap_or(&file.path);
        let name = relative.display().to_string();
        let unsaved = super::files::unsaved_under(cx.editor, &file.path);
        let lines = discard_question(&name, file.is_untracked(), unsaved);
        let answers = vec![
            Answer::new("Cancel", Box::new(|_| {})),
            Answer::new(
                "Discard",
                Box::new(move |cx| {
                    // The answer runs with no sidebar in reach: the act goes through a job of
                    // the editor's, once the box's own turn is over.
                    let callback = Box::pin(async move {
                        let call = job::Callback::EditorCompositor(Box::new(
                            move |_editor, compositor| {
                                if let Some(view) = compositor.find::<EditorView>() {
                                    view.sidebar.changes.act(Act::Discard, file);
                                }
                            },
                        ));
                        Ok(call)
                    });
                    cx.jobs.callback(callback);
                }),
            )
            .destructive(),
        ];
        cx.push_layer(Box::new(Confirm::new("Confirm", lines, answers)));
    }
}

/// One hunk of the uncommitted diff on screen is staged, unstaged or discarded: git is
/// asked for the file's hunks afresh, the one under the cursor found by its line, and
/// applied on its own. The list and the diff are asked again when it is done; a file
/// whose hunk was discarded is read again from disk if it is open. An untracked file is
/// one hunk: it is staged whole, and never discarded here — that deletes it, and is
/// asked about as the whole file's discard is.
pub fn act_on_hunk(sidebar: &mut super::Sidebar, editor: &mut Editor, act: git::HunkAct) {
    let Some((file, at)) = sidebar.diff.working_hunk(editor) else {
        editor.set_error("Put the cursor on a line of an uncommitted diff to act on its hunk");
        return;
    };
    if file.is_untracked() {
        match act {
            git::HunkAct::Stage => sidebar.changes.act(Act::Stage, file),
            git::HunkAct::Unstage => {
                editor.set_error("Git does not have the file: nothing is staged")
            }
            git::HunkAct::Discard => {
                editor.set_error("Git does not have the file: discard it from the Changes tab")
            }
        }
        return;
    }
    let root = sidebar.changes.root.clone();
    super::background(
        move || {
            let staged = act == git::HunkAct::Unstage;
            let (header, hunks) = git::file_hunks(&root, &file, staged)?;
            // Unstaging reads the index against the last commit, whose lines are the old
            // side's; the other two read the working tree, the new side.
            let line = if staged { at.old } else { at.new };
            let hunk = line
                .and_then(|line| git::hunk_holding(&hunks, staged, line))
                .ok_or("No hunk under the cursor")?;
            git::apply_hunk(&root, &header, hunk, act)?;
            Ok::<_, String>(file)
        },
        move |sidebar, editor, answer| {
            match answer {
                Err(err) => editor.set_error(err),
                Ok(file) => {
                    if act == git::HunkAct::Discard {
                        reload_document(editor, &file.path);
                    }
                    // The reader may have left the diff meanwhile: nothing to read again.
                    if sidebar.diff.is_on_screen(editor) {
                        sidebar.diff.refresh(editor);
                    }
                }
            }
            sidebar.changes.ask();
        },
    );
}

/// Asks before one hunk of `file` is thrown away, in the usual box.
pub fn confirm_discard_hunk(cx: &mut commands::Context, root: &Path, file: &ChangedFile) {
    let relative = file.path.strip_prefix(root).unwrap_or(&file.path);
    let name = relative.display().to_string();
    let mut lines = vec![
        format!("Discard this hunk of \"{name}\"?"),
        "The change under the cursor is lost; the rest of the file stays.".to_string(),
    ];
    if super::files::unsaved_under(cx.editor, &file.path) {
        lines.push("Unsaved changes to the file in the editor will be lost too.".to_string());
    }
    let answers = vec![
        Answer::new("Cancel", Box::new(|_| {})),
        Answer::new(
            "Discard",
            Box::new(move |cx| {
                let callback = Box::pin(async move {
                    let call =
                        job::Callback::EditorCompositor(Box::new(move |editor, compositor| {
                            if let Some(view) = compositor.find::<EditorView>() {
                                act_on_hunk(&mut view.sidebar, editor, git::HunkAct::Discard);
                            }
                        }));
                    Ok(call)
                });
                cx.jobs.callback(callback);
            }),
        )
        .destructive(),
    ];
    cx.push_layer(Box::new(Confirm::new("Confirm", lines, answers)));
}

/// What the box asks before `name` is discarded: deleted when git does not have it,
/// restored otherwise, and, when a buffer holds edits to it not saved yet, that those go
/// too.
fn discard_question(name: &str, untracked: bool, unsaved: bool) -> Vec<String> {
    let mut lines = if untracked {
        vec![
            format!("Delete \"{name}\"?"),
            "Git does not have it: there is nothing to get it back from.".to_string(),
        ]
    } else {
        vec![
            format!("Discard the changes to \"{name}\"?"),
            "What is not staged is lost.".to_string(),
        ]
    };
    if unsaved {
        lines.push("Unsaved changes to it in the editor will be lost too.".to_string());
    }
    lines
}

/// Reads the document open on `path` again from disk, the way `:reload` does, after git
/// changed what is there.
fn reload_document(editor: &mut Editor, path: &Path) {
    let Some(doc) = editor.documents().find(|doc| doc.path() == Some(path)) else {
        return;
    };
    let doc_id = doc.id();
    let trust_full = editor
        .workspace_trust
        .query(
            doc.workspace_root(),
            helix_loader::workspace_trust::TrustQuery::Git,
        )
        .is_trusted();
    let scrolloff = editor.config().scrolloff;
    let focused = view!(editor).id;
    let mut view_ids: Vec<helix_view::ViewId> = doc.selections().keys().cloned().collect();
    if view_ids.is_empty() {
        view_ids.push(focused);
    }
    let doc = doc_mut!(editor, &doc_id);
    doc.ensure_view_init(view_ids[0]);
    let view = view_mut!(editor, view_ids[0]);
    view.sync_changes(doc);
    if let Err(err) = doc.reload(view, &editor.diff_providers, trust_full) {
        editor.set_error(format!("{}: {err}", path.display()));
        return;
    }
    editor
        .language_servers
        .file_event_handler
        .file_changed(path.to_path_buf());
    for view_id in view_ids {
        let view = view_mut!(editor, view_id);
        if view.doc == doc_id {
            view.sync_changes(doc);
            view.ensure_cursor_in_view(doc, scrolloff);
        }
    }
}

impl TabView for ChangesTab {
    fn label(&self) -> String {
        match self.count() {
            0 => "Changes".to_string(),
            count => format!("Changes {count}"),
        }
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

    fn empty_message(&self) -> Option<Message> {
        let (text, is_error) = match &self.answer {
            None => ("reading git status…".to_string(), false),
            Some(Ok(_)) => ("no changes".to_string(), false),
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
        if let Some(Ok(files)) = &self.answer {
            entries::list_changed(&self.root, files, &self.folds, &mut rows);
        }
        self.rows = rows;
        entries::reselect(&self.rows, &mut self.list, selected.as_deref());
    }

    fn shown(&mut self, _cx: &mut TabContext) {
        self.ask_if_idle();
    }

    fn refresh(&mut self, _cx: &mut TabContext) {
        self.ask();
    }

    /// A click shows what the file changed; Enter or a double click goes over to read it.
    /// `o` opens the file itself.
    fn open(&mut self, cx: &mut TabContext, how: Activation) -> Outcome {
        if !self.show_diff(cx) {
            return Outcome::Stay;
        }
        match how {
            Activation::Click => Outcome::Stay,
            Activation::Enter | Activation::Double => Outcome::Leave,
        }
    }

    /// While a diff is on screen, it follows the cursor once the cursor rests.
    fn cursor_moved(&mut self, cx: &mut TabContext) {
        if !cx.diff.is_on_screen(cx.editor) {
            return;
        }
        self.moves = self.moves.wrapping_add(1);
        let move_number = self.moves;
        super::later(PREVIEW_DELAY, move |sidebar, editor| {
            if sidebar.changes.moves != move_number || !sidebar.showing(TabKind::Changes) {
                return;
            }
            let mut cx = TabContext {
                editor,
                diff: &mut sidebar.diff,
            };
            sidebar.changes.show_diff(&mut cx);
        });
    }

    fn reveal(&mut self, _editor: &mut Editor, path: &Path) {
        self.centered = Some(path.to_path_buf());
        entries::reselect(&self.rows, &mut self.list, Some(path));
        self.list.center();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tree_finds_a_changed_file_and_the_directories_above_it() {
        let root = PathBuf::from("/p");
        let mut tab = ChangesTab::new(root.clone());
        tab.answer = Some(Ok(vec![ChangedFile {
            path: root.join("src/ui/a.rs"),
            change: Change::Modified,
            from: None,
            staged: None,
            unstaged: Some(Change::Modified),
            lines: None,
        }]));
        tab.index_answer();
        assert!(matches!(
            tab.mark(&root.join("src/ui/a.rs"), false),
            Some(Mark::File(file)) if file.change == Change::Modified
        ));
        assert!(matches!(
            tab.mark(&root.join("src/ui"), true),
            Some(Mark::Dir)
        ));
        assert!(matches!(tab.mark(&root.join("src"), true), Some(Mark::Dir)));
        // The root itself, and what holds no change, wear nothing.
        assert!(tab.mark(&root, true).is_none());
        assert!(tab.mark(&root.join("docs"), true).is_none());
        assert!(tab.mark(&root.join("src/b.rs"), false).is_none());
    }

    #[test]
    fn the_question_says_when_unsaved_edits_go_too() {
        let asked = discard_question("a.rs", false, false);
        assert_eq!(asked.len(), 2);
        assert!(asked[0].starts_with("Discard the changes"));

        // Whether the file is deleted or restored, edits not saved yet are named.
        for untracked in [true, false] {
            let asked = discard_question("a.rs", untracked, true);
            assert_eq!(asked.len(), 3);
            assert!(asked[2].contains("Unsaved changes"));
        }
        assert!(discard_question("a.rs", true, false)[0].starts_with("Delete"));
    }
}
