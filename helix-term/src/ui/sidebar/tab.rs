//! What every tab of the sidebar answers to. The sidebar itself moves the cursor, folds
//! directories, draws the rows and routes the keys; a tab says what its rows are, where
//! they come from, and what opening one means.

use std::path::{Path, PathBuf};

use helix_view::Editor;

use super::diff_view::DiffView;
use super::entries::{Folds, Row};
use super::list::List;

/// What a tab may reach while it acts: the editor, and the buffer the sidebar shows diffs
/// in, which every tab shares so two never fight over it.
pub struct TabContext<'a> {
    pub editor: &'a mut Editor,
    pub diff: &'a mut DiffView,
}

/// How the row under the cursor was asked to open.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    /// Enter, `l` or `→`: the row is meant.
    Enter,
    /// A double click: the row is meant, and the same click again undoes it where the tab
    /// has something to open and close.
    Double,
    /// A single click: the row is chosen, which for some tabs is enough.
    Click,
}

/// Whether the keys go over to the editor after a row was opened.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Stay,
    Leave,
}

/// What a tab says in place of its rows while it has none.
pub struct Message {
    pub text: String,
    pub is_error: bool,
}

pub trait TabView {
    /// The name on the tab strip; it may carry a count or what the tab is narrowed to.
    fn label(&self) -> String;

    fn rows(&self) -> &[Row];

    fn list(&self) -> &List;

    fn list_mut(&mut self) -> &mut List;

    /// The directories' open state, for a tab that lists directories.
    fn folds(&self) -> Option<&Folds> {
        None
    }

    fn folds_mut(&mut self) -> Option<&mut Folds> {
        None
    }

    fn empty_message(&self) -> Option<Message> {
        None
    }

    /// Whether the row at `index` folds — a directory, a definition with others inside
    /// it — and whether it is open; none for a row that cannot fold.
    fn fold_state(&self, index: usize) -> Option<bool> {
        let dir = self.rows().get(index)?.dir()?;
        Some(self.folds()?.is_open(dir))
    }

    /// Opens or closes the row at `index`, when it folds.
    fn set_fold(&mut self, editor: &mut Editor, index: usize, open: bool) {
        let dir = self
            .rows()
            .get(index)
            .and_then(Row::dir)
            .map(Path::to_path_buf);
        let Some(dir) = dir else {
            return;
        };
        let Some(folds) = self.folds_mut() else {
            return;
        };
        folds.set(dir, open);
        self.rebuild(editor);
    }

    /// Closes every row that folds, or opens them all. A tree that reads the disk as
    /// directories are opened only closes: opening every directory would read it whole.
    fn fold_all(&mut self, editor: &mut Editor, open: bool) {
        if open {
            return;
        }
        let dirs: Vec<PathBuf> = self
            .rows()
            .iter()
            .filter_map(Row::dir)
            .map(Path::to_path_buf)
            .collect();
        let Some(folds) = self.folds_mut() else {
            return;
        };
        folds.close_all(dirs.into_iter());
        self.rebuild(editor);
    }

    /// Lays the rows out again from what the tab holds; the cursor stays on its entry.
    fn rebuild(&mut self, editor: &mut Editor);

    /// The tab came on screen: the sidebar was shown or focused with it, or it was switched
    /// to. A tab that asks git asks now, unless it is already asking.
    fn shown(&mut self, cx: &mut TabContext) {
        let _ = cx;
    }

    /// `R`: whatever the tab shows is asked for again.
    fn refresh(&mut self, cx: &mut TabContext);

    /// The cursor moved, by a key, the wheel or a click.
    fn cursor_moved(&mut self, cx: &mut TabContext) {
        let _ = cx;
    }

    /// `Esc`: a tab with somewhere to step back to does, and says so; otherwise the keys
    /// go back to the editor.
    fn step_back(&mut self, cx: &mut TabContext) -> bool {
        let _ = cx;
        false
    }

    /// The row under the cursor was asked to open; a directory never reaches here, the
    /// sidebar folds it itself.
    fn open(&mut self, cx: &mut TabContext, how: Activation) -> Outcome;

    /// The focused document changed: a tab that shows the disk moves onto it.
    fn reveal(&mut self, editor: &mut Editor, path: &Path) {
        let _ = (editor, path);
    }

    /// Whether `a`, `r` and `d` act on the rows, which they do when the rows are files on
    /// disk and not in a commit.
    fn edits_disk(&self) -> bool {
        false
    }
}
