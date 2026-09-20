//! The buffer the sidebar shows a diff in: one scratch buffer, named for what it shows,
//! in the focused view. Side by side it is two: the old side in the focused view and the
//! new one in a view split to its right, scrolled together.

use helix_core::syntax::Loader;
use std::path::PathBuf;
use std::sync::Arc;

use helix_core::{Selection, Transaction};
use helix_view::editor::{Action, CloseError};
use helix_view::review::{HunkAt, ReviewAnchor};
use helix_view::view::ViewPosition;
use helix_view::{DocumentId, Editor, ViewId};

use super::{git, review};

/// Where a patch comes from: a commit, or what a file changed and nobody committed yet.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DiffSource {
    Commit(String),
    WorkingTree(git::ChangedFile),
}

/// What the buffer is asked to show: a patch narrowed to some paths, and the name the
/// buffer goes by while it shows it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DiffTarget {
    pub source: DiffSource,
    pub pathspecs: Vec<String>,
    pub name: String,
    /// Whether the commit's message goes above the patch: it does for a commit looked at
    /// whole, not for one file of it, which is read for its own sake.
    pub describe: bool,
}

/// How a target is asked: its patch, with the whole file around the changes or not, one
/// side above the other or beside it.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Request {
    target: DiffTarget,
    full_context: bool,
    side_by_side: bool,
}

/// A patch ready to show: one buffer, or the old side and the new.
enum Shown {
    Stacked(review::ParsedReview),
    Beside([review::ParsedReview; 2]),
}

/// Where a view of one side is scrolled to: the row on top, and how far right.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Scroll {
    row: usize,
    column: usize,
}

pub struct DiffView {
    root: PathBuf,
    /// The buffer, reused for as long as it lives: helix drops an untouched scratch buffer
    /// as soon as a view leaves it, and then it is made again. Side by side, the old side.
    doc: Option<DocumentId>,
    /// The new side's buffer, while the diff is shown side by side.
    new_side: Option<DocumentId>,
    /// What was last asked for, so the same request is not asked twice, and an answer to
    /// something asked before the cursor moved on is dropped.
    asked: Option<Request>,
    /// The request actually displayed. A newer request may still be reading git.
    displayed: Option<Request>,
    full_context: bool,
    side_by_side: bool,
    /// Where both sides were last left scrolled, so the one scrolled since leads the other.
    scrolled: Option<Scroll>,
}

impl DiffView {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            doc: None,
            new_side: None,
            asked: None,
            displayed: None,
            full_context: false,
            side_by_side: false,
            scrolled: None,
        }
    }

    /// Asks git for the target's patch, unless it is the one already asked for. The patch
    /// lands in the buffer when it comes, if the target is still the one wanted.
    pub fn ask(&mut self, target: DiffTarget, loader: Arc<Loader>) {
        self.ask_at(target, loader, None);
    }

    fn ask_at(&mut self, target: DiffTarget, loader: Arc<Loader>, anchor: Option<ReviewAnchor>) {
        let request = Request {
            target,
            full_context: self.full_context,
            side_by_side: self.side_by_side,
        };
        if self.asked.as_ref() == Some(&request) {
            return;
        }
        self.asked = Some(request.clone());
        let root = self.root.clone();
        let asked = request.clone();
        super::background(
            move || {
                let target = &asked.target;
                let patch = match &target.source {
                    DiffSource::Commit(hash) => {
                        git::show(&root, hash, &target.pathspecs, asked.full_context)?
                    }
                    DiffSource::WorkingTree(file) => {
                        git::working_diff(&root, file, asked.full_context)?
                    }
                };
                let mut parsed = review::parse(&patch)?;
                if let DiffSource::Commit(hash) = &target.source {
                    if target.describe {
                        parsed.prepend_commit(&git::commit_text(&root, hash)?);
                    }
                }
                if !asked.side_by_side {
                    parsed.review.prepare_syntax(&loader);
                    return Ok(Shown::Stacked(parsed));
                }
                let mut sides = parsed.split();
                for side in &mut sides {
                    side.review.prepare_syntax(&loader);
                }
                Ok(Shown::Beside(sides))
            },
            move |sidebar, editor, answer| sidebar.diff.landed(editor, request, anchor, answer),
        );
    }

    /// Whether `id` is one of the diff's buffers.
    fn shows(&self, id: DocumentId) -> bool {
        self.doc == Some(id) || self.new_side == Some(id)
    }

    /// Whether the focused view shows the diff buffer.
    pub fn is_on_screen(&self, editor: &Editor) -> bool {
        self.shows(view!(editor).doc)
    }

    /// The line under the cursor of the diff buffer, when it shows one, as a line to blame:
    /// in the text the commit or the working tree leaves, or, for a removed line, the text
    /// it was removed from.
    pub fn blame_request(&self, editor: &Editor) -> Option<git::BlameRequest> {
        let (view, doc) = current_ref!(editor);
        let review = doc.review.as_ref().filter(|_| self.shows(doc.id()))?;
        let target = &self.displayed.as_ref()?.target;
        let row = doc
            .selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..));
        let (path, line, old) = review.file_line(row)?;
        let (path, text) = match (&target.source, old) {
            (DiffSource::Commit(hash), false) => {
                (path.to_path_buf(), git::BlameText::Revision(hash.clone()))
            }
            (DiffSource::Commit(hash), true) => (
                path.to_path_buf(),
                git::BlameText::Revision(format!("{hash}^")),
            ),
            // A file git does not know is named from the workspace, not the repository's top.
            (DiffSource::WorkingTree(file), false) => (file.path.clone(), git::BlameText::Disk),
            (DiffSource::WorkingTree(_), true) => (
                path.to_path_buf(),
                git::BlameText::Revision("HEAD".to_string()),
            ),
        };
        Some(git::BlameRequest {
            path,
            line: line - 1,
            text,
        })
    }

    /// The uncommitted file whose diff the focused view shows, and where its cursor is in
    /// that file, for git to find the hunk under it; none when the focused view shows
    /// something else, a commit's diff, or a heading.
    pub fn working_hunk(&self, editor: &Editor) -> Option<(git::ChangedFile, HunkAt)> {
        let (view, doc) = current_ref!(editor);
        let review = doc.review.as_ref().filter(|_| self.shows(doc.id()))?;
        let target = &self.displayed.as_ref()?.target;
        let DiffSource::WorkingTree(file) = &target.source else {
            return None;
        };
        let row = doc
            .selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..));
        Some((file.clone(), review.hunk_at(row)?))
    }

    /// Asks git again for the diff on screen, keeping the line under the cursor, after
    /// the file or the index changed under it.
    pub fn refresh(&mut self, editor: &mut Editor) {
        self.ask_again(editor, "Open a diff to read it again", |_| {});
    }

    pub fn full_context(&self) -> bool {
        self.full_context
    }

    pub fn side_by_side(&self) -> bool {
        self.side_by_side
    }

    pub fn toggle_context(&mut self, editor: &mut Editor) {
        self.ask_again(editor, "Open a commit diff to change its context", |diff| {
            diff.full_context = !diff.full_context;
        });
    }

    pub fn toggle_side_by_side(&mut self, editor: &mut Editor) {
        self.ask_again(editor, "Open a diff to show it side by side", |diff| {
            diff.side_by_side = !diff.side_by_side;
        });
    }

    /// Asks the diff on screen again, changed by `change`, keeping the line under the
    /// cursor; asked even when nothing changed, since the file may have.
    fn ask_again(&mut self, editor: &mut Editor, missing: &'static str, change: fn(&mut Self)) {
        let (view, doc) = current_ref!(editor);
        let Some(review) = doc.review.as_ref().filter(|_| self.shows(doc.id())) else {
            editor.set_error(missing);
            return;
        };
        let row = doc
            .selection(view.id)
            .primary()
            .cursor_line(doc.text().slice(..));
        let anchor = review.anchor(row);
        let Some(request) = self.displayed.clone() else {
            return;
        };
        self.forget();
        change(self);
        let loader = editor.syn_loader.load_full();
        self.ask_at(request.target, loader, anchor);
    }

    /// Forgets what was asked, so the next ask goes to git even for the same target: the
    /// buffer may have been left for another one meanwhile.
    pub fn forget(&mut self) {
        self.asked = None;
    }

    fn landed(
        &mut self,
        editor: &mut Editor,
        request: Request,
        anchor: Option<ReviewAnchor>,
        answer: git::Answer<Shown>,
    ) {
        if self.asked.as_ref() != Some(&request) {
            return;
        }
        match answer {
            Ok(shown) => {
                self.displayed = Some(request.clone());
                self.show(editor, request.target.name, shown, anchor);
            }
            Err(err) => editor.set_error(err),
        }
    }

    /// Puts the diff in its buffers under `name`, from its first line or from `anchor`: the
    /// one buffer in the focused view, or the old side there and the new one beside it.
    fn show(
        &mut self,
        editor: &mut Editor,
        name: String,
        shown: Shown,
        anchor: Option<ReviewAnchor>,
    ) {
        // The old side goes where it already is, never over the new side's view.
        if self.new_side == Some(view!(editor).doc) {
            if let Some(old) = self.doc.and_then(|doc| view_showing(editor, doc)) {
                editor.focus(old);
            }
        }
        let (parsed, new_side) = match shown {
            Shown::Stacked(parsed) => (parsed, None),
            Shown::Beside([old, new]) => (old, Some(new)),
        };
        let line = anchor
            .as_ref()
            .and_then(|anchor| parsed.review.find_anchor(anchor));
        let live = self.doc.filter(|id| editor.documents.contains_key(id));
        let id = match live {
            Some(id) => id,
            None => editor.new_file(Action::Replace),
        };
        self.doc = Some(id);
        if view!(editor).doc != id {
            editor.switch(id, Action::Replace);
        }
        let old_view = view!(editor).id;
        self.scrolled = None;
        let Some(new_side) = new_side else {
            fill(editor, old_view, id, name, parsed, line);
            if let Some(beside) = self.new_side.take() {
                match editor.close_document(beside, true) {
                    Ok(()) | Err(CloseError::DoesNotExist) => {}
                    Err(CloseError::BufferModified(name)) => {
                        editor.set_error(format!("{name} is modified and stays open"));
                    }
                    Err(CloseError::SaveError(err)) => editor.set_error(err.to_string()),
                }
            }
            return;
        };
        fill(
            editor,
            old_view,
            id,
            format!("{name} · before"),
            parsed,
            line,
        );
        let live = self.new_side.filter(|id| editor.documents.contains_key(id));
        let new_view = match live.and_then(|id| view_showing(editor, id)) {
            Some(view) => view,
            None => {
                match live {
                    Some(id) => editor.switch(id, Action::VerticalSplit),
                    None => {
                        editor.new_file(Action::VerticalSplit);
                    }
                }
                let view = view!(editor);
                self.new_side = Some(view.doc);
                let view = view.id;
                editor.focus(old_view);
                view
            }
        };
        let new_id = editor.tree.get(new_view).doc;
        fill(
            editor,
            new_view,
            new_id,
            format!("{name} · after"),
            new_side,
            line,
        );
    }

    /// Keeps the two sides of a diff shown side by side on the same rows: the side scrolled
    /// or moved since the last frame leads, and the other follows it to the same top row,
    /// the same column and the same cursor line. Run before the views are drawn.
    pub fn follow(&mut self, editor: &mut Editor) {
        let (Some(old), Some(new)) = (self.doc, self.new_side) else {
            return;
        };
        let (Some(old_view), Some(new_view)) =
            (view_showing(editor, old), view_showing(editor, new))
        else {
            return;
        };
        let views = [(old_view, old), (new_view, new)];
        let scrolls = views.map(|(view, doc)| scroll_of(editor, view, doc));
        let focused = views
            .iter()
            .position(|(view, _)| *view == editor.tree.focus);
        let leader = leading(self.scrolled, scrolls, focused);
        let (lead, follower) = match leader {
            Some(0) => (views[0], views[1]),
            Some(_) => (views[1], views[0]),
            // Nothing scrolled: the cursor still follows the side it moves in.
            None => match focused {
                Some(0) => (views[0], views[1]),
                Some(_) => (views[1], views[0]),
                None => return,
            },
        };
        let lead_doc = &editor.documents[&lead.1];
        let position = lead_doc.view_offset(lead.0);
        let cursor = lead_doc
            .selection(lead.0)
            .primary()
            .cursor_line(lead_doc.text().slice(..));
        let scroll = scroll_of(editor, lead.0, lead.1);
        let doc = doc_mut!(editor, &follower.1);
        let text = doc.text().clone();
        let last = text.len_lines().saturating_sub(1);
        let current = doc
            .selection(follower.0)
            .primary()
            .cursor_line(text.slice(..));
        if current != cursor {
            let pos = text.line_to_char(cursor.min(last));
            doc.set_selection(follower.0, Selection::point(pos));
        }
        if leader.is_some() {
            let anchor = text.line_to_char(scroll.row.min(last));
            doc.set_view_offset(
                follower.0,
                ViewPosition {
                    anchor,
                    horizontal_offset: position.horizontal_offset,
                    vertical_offset: position.vertical_offset,
                },
            );
        }
        self.scrolled = Some(scroll);
    }
}

/// Which of two sides leads, given where both were last left and where each is now: the
/// one that moved; when both did, the focused one, else the first.
fn leading(last: Option<Scroll>, now: [Scroll; 2], focused: Option<usize>) -> Option<usize> {
    let moved = now.map(|scroll| last != Some(scroll));
    match moved {
        [false, false] => None,
        [true, false] => Some(0),
        [false, true] => Some(1),
        [true, true] if now[0] == now[1] => None,
        [true, true] => Some(focused.unwrap_or(0)),
    }
}

fn scroll_of(editor: &Editor, view: ViewId, doc: DocumentId) -> Scroll {
    let doc = &editor.documents[&doc];
    let position = doc.view_offset(view);
    let text = doc.text();
    Scroll {
        row: text.char_to_line(position.anchor.min(text.len_chars())),
        column: position.horizontal_offset,
    }
}

/// The view that shows `doc`, if one does.
fn view_showing(editor: &Editor, doc: DocumentId) -> Option<ViewId> {
    editor
        .tree
        .views()
        .find(|(view, _)| view.doc == doc)
        .map(|(view, _)| view.id)
}

/// Puts `parsed` in the buffer `id` shown by `view_id`, under `name`, with the cursor and
/// the top of the view on `line` when there is one.
fn fill(
    editor: &mut Editor,
    view_id: ViewId,
    id: DocumentId,
    name: String,
    parsed: review::ParsedReview,
    line: Option<usize>,
) {
    let view = editor.tree.get_mut(view_id);
    let doc = doc_mut!(editor, &id);
    doc.review = None;
    let length = doc.text().len_chars();
    let change = (0, length, Some(parsed.text.into()));
    let transaction = Transaction::change(doc.text(), std::iter::once(change))
        .with_selection(Selection::point(0));
    doc.apply(&transaction, view.id);
    doc.append_changes_to_history(view);
    doc.reset_modified();
    if let Some(line) = line {
        let pos = doc.text().line_to_char(line);
        doc.set_selection(view.id, Selection::point(pos));
    }
    doc.set_view_offset(
        view.id,
        ViewPosition {
            anchor: line
                .map(|line| doc.text().line_to_char(line.saturating_sub(3)))
                .unwrap_or(0),
            ..ViewPosition::default()
        },
    );
    doc.scratch_name = Some(name);
    doc.readonly = true;
    doc.review = Some(parsed.review);
}

#[cfg(test)]
mod tests {
    use super::{leading, Scroll};

    #[cfg(feature = "integration")]
    #[tokio::test(flavor = "multi_thread")]
    async fn actions_use_the_displayed_diff_while_another_is_pending() {
        use super::*;
        let mut app = super::super::tests::test_app();
        let mut diff = DiffView::new(PathBuf::from("/repo"));
        let file = git::ChangedFile {
            path: PathBuf::from("/repo/a"),
            change: git::Change::Modified,
            from: None,
            staged: None,
            unstaged: Some(git::Change::Modified),
        };
        let displayed = Request {
            target: DiffTarget {
                source: DiffSource::WorkingTree(file.clone()),
                pathspecs: vec![],
                name: "a".into(),
                describe: false,
            },
            full_context: false,
            side_by_side: false,
        };
        diff.asked = Some(displayed.clone());
        let parsed =
            review::parse("diff --git a/a b/a\n--- a/a\n+++ b/a\n@@ -1 +1 @@\n-old\n+new\n")
                .unwrap();
        let row = parsed
            .review
            .lines
            .iter()
            .position(|line| line.new == Some(1))
            .unwrap();
        diff.landed(
            &mut app.editor,
            displayed.clone(),
            None,
            Ok(Shown::Stacked(parsed)),
        );
        let (view, doc) = current!(app.editor);
        doc.set_selection(view.id, Selection::point(doc.text().line_to_char(row)));
        let mut pending = displayed;
        let DiffSource::WorkingTree(file) = &mut pending.target.source else {
            unreachable!()
        };
        file.path = PathBuf::from("/repo/b");
        diff.asked = Some(pending);
        assert_eq!(
            diff.working_hunk(&app.editor).unwrap().0.path,
            PathBuf::from("/repo/a")
        );
        assert_eq!(
            diff.blame_request(&app.editor).unwrap().path,
            PathBuf::from("/repo/a")
        );
        assert!(app.close().await.is_empty());
    }

    fn at(row: usize) -> Scroll {
        Scroll { row, column: 0 }
    }

    #[test]
    fn the_side_scrolled_leads_the_other() {
        // The wheel over either side moves it alone: that side leads.
        assert_eq!(leading(Some(at(10)), [at(13), at(10)], Some(1)), Some(0));
        assert_eq!(leading(Some(at(10)), [at(10), at(7)], Some(0)), Some(1));
        // Scrolled right is a move too.
        let right = Scroll { row: 10, column: 4 };
        assert_eq!(leading(Some(at(10)), [at(10), right], None), Some(1));
        // Nothing moved, or both already together: nobody leads.
        assert_eq!(leading(Some(at(10)), [at(10), at(10)], Some(0)), None);
        assert_eq!(leading(None, [at(0), at(0)], Some(1)), None);
        // Both moved apart, as when a diff has just landed: the focused side leads.
        assert_eq!(leading(None, [at(4), at(9)], Some(1)), Some(1));
        assert_eq!(leading(Some(at(0)), [at(4), at(9)], None), Some(0));
    }
}
