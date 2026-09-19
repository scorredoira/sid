//! The editor's own git commands: what a buffer's file has been through, and who changed
//! the line under the cursor. They answer in the status line and, for more, in the
//! sidebar.

use std::path::{Path, PathBuf};

use super::Context;
use crate::ui::sidebar::git::{self, Blame, BlameRequest, BlameText};
use crate::ui::{self, EditorView};

/// The line blamed last, so blaming it again opens its commit.
pub struct LastBlame {
    path: PathBuf,
    line: usize,
    revision: Option<String>,
    blame: Blame,
}

/// Shows the history of the current file in the sidebar.
pub fn file_history(cx: &mut Context) {
    let Some(path) = doc!(cx.editor).path().map(Path::to_path_buf) else {
        cx.editor
            .set_error("The buffer has no file to show the history of");
        return;
    };
    cx.callback.push(Box::new(move |compositor, cx| {
        let view = compositor.find::<EditorView>().unwrap();
        view.sidebar.show_history(cx.editor, path);
    }));
}

/// Says who last changed the line under the cursor, in which commit and when, counting the
/// buffer's unsaved text, or, in a diff, in the text the diff shows the line in; asked again
/// on the same line, opens that commit in the sidebar.
pub fn blame_line(cx: &mut Context) {
    let (view, doc) = current_ref!(cx.editor);
    let root = doc.workspace_root().to_path_buf();
    let in_review = doc.review.is_some();
    let buffer = doc.path().map(|path| {
        let text = doc.text().slice(..);
        BlameRequest {
            path: path.to_path_buf(),
            line: doc.selection(view.id).primary().cursor_line(text),
            text: BlameText::Buffer(text.to_string()),
        }
    });
    cx.callback.push(Box::new(move |compositor, cx| {
        let view = compositor.find::<EditorView>().unwrap();
        let (root, request) = match buffer {
            Some(request) => (root, request),
            None => match view.sidebar.diff_blame_request(cx.editor) {
                Some(found) => found,
                None if in_review => {
                    cx.editor
                        .set_error("Put the cursor on a line of code to blame it");
                    return;
                }
                None => {
                    cx.editor.set_error("The buffer has no file to blame");
                    return;
                }
            },
        };
        let same_line = view.last_blame.as_ref().filter(|last| {
            last.path == request.path
                && last.line == request.line
                && last.revision.as_deref() == request.text.revision()
        });
        if let Some(last) = same_line {
            match last.blame.commit.clone() {
                Some(commit) => view.sidebar.open_commit(commit),
                None => cx.editor.set_status("Not committed yet"),
            }
            return;
        }
        ui::editor::background(
            move || {
                let blame = git::blame(&root, &request);
                (request, blame)
            },
            |editor, view, (request, blame)| {
                let blame = match blame {
                    Ok(blame) => blame,
                    Err(err) => {
                        editor.set_error(err);
                        return;
                    }
                };
                match &blame.commit {
                    Some(commit) => editor.set_status(format!(
                        "{} · {} · {} ago · {} (blame again to open it)",
                        commit.short,
                        blame.author,
                        ui::sidebar::format_age(commit.time),
                        commit.subject
                    )),
                    None => editor.set_status("Not committed yet"),
                }
                view.last_blame = Some(LastBlame {
                    revision: request.text.revision().map(str::to_string),
                    path: request.path,
                    line: request.line,
                    blame,
                });
            },
        );
    }));
}

/// Review controls are commands so menus, configured keys and the palette share them.
pub fn review_cycle(cx: &mut Context) {
    cx.callback.push(Box::new(|compositor, cx| {
        let view = compositor.find::<EditorView>().unwrap();
        view.sidebar.cycle_review(cx.editor);
    }));
}

pub fn review_commits_toggle(cx: &mut Context) {
    cx.callback.push(Box::new(|compositor, cx| {
        let view = compositor.find::<EditorView>().unwrap();
        view.sidebar.toggle_commits(cx.editor);
    }));
}

pub fn review_code_toggle(cx: &mut Context) {
    cx.callback.push(Box::new(|compositor, cx| {
        let view = compositor.find::<EditorView>().unwrap();
        view.sidebar.toggle_code(cx.editor);
    }));
}

pub fn review_files_toggle(cx: &mut Context) {
    cx.callback.push(Box::new(|compositor, cx| {
        let view = compositor.find::<EditorView>().unwrap();
        view.sidebar.toggle_commit_files(cx.editor);
    }));
}

pub fn review_context_toggle(cx: &mut Context) {
    cx.callback.push(Box::new(|compositor, cx| {
        let view = compositor.find::<EditorView>().unwrap();
        view.sidebar.toggle_context(cx.editor);
    }));
}

pub fn review_side_by_side_toggle(cx: &mut Context) {
    cx.callback.push(Box::new(|compositor, cx| {
        let view = compositor.find::<EditorView>().unwrap();
        view.sidebar.toggle_side_by_side(cx.editor);
    }));
}
