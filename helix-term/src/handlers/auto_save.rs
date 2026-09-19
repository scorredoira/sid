use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::Ok;
use arc_swap::access::Access;

use helix_event::{register_hook, send_blocking};
use helix_view::{
    doc_mut,
    document::Mode,
    events::{DocumentDidChange, DocumentFocusLost},
    handlers::{AutoSaveEvent, Handlers},
    DocumentId, Editor,
};
use tokio::time::Instant;

use crate::{
    commands, compositor,
    events::OnModeSwitch,
    job::{self, Jobs},
};

#[derive(Debug)]
pub(super) struct AutoSaveHandler {
    /// The documents changed since the last delayed save, waiting for it: only those are
    /// written, not every modified document there is. Shared with the save itself, which
    /// leaves them in place when it has to wait for the insert session to end.
    pending: Arc<Mutex<Vec<DocumentId>>>,
}

impl AutoSaveHandler {
    pub fn new() -> AutoSaveHandler {
        AutoSaveHandler {
            pending: Default::default(),
        }
    }
}

impl helix_event::AsyncHook for AutoSaveHandler {
    type Event = AutoSaveEvent;

    fn handle_event(
        &mut self,
        event: Self::Event,
        existing_debounce: Option<tokio::time::Instant>,
    ) -> Option<Instant> {
        match event {
            Self::Event::DocumentChanged { save_after, doc } => {
                let mut pending = self.pending.lock().unwrap();
                if !pending.contains(&doc) {
                    pending.push(doc);
                }
                Some(Instant::now() + Duration::from_millis(save_after))
            }
            Self::Event::LeftInsertMode => {
                if existing_debounce.is_some() {
                    // If the change happened more recently than the debounce, let the
                    // debounce run down before saving.
                    existing_debounce
                } else {
                    // Otherwise if there is a save pending, save immediately.
                    if !self.pending.lock().unwrap().is_empty() {
                        self.finish_debounce();
                    }
                    None
                }
            }
        }
    }

    fn finish_debounce(&mut self) {
        let pending = self.pending.clone();
        job::dispatch_blocking(move |editor, _| {
            if editor.mode() == Mode::Insert && editor.config().default_mode != Mode::Insert {
                // Modal editing waits for the insert session to finish. In sid's
                // default mode it never finishes; write_documents_impl commits the
                // pending edits to history before saving, keeping saved revisions
                // accurate.
                return;
            }
            let docs: Vec<DocumentId> = std::mem::take(&mut *pending.lock().unwrap());
            request_auto_save(editor, docs);
        })
    }
}

fn request_auto_save(editor: &mut Editor, docs: Vec<DocumentId>) {
    let context = &mut compositor::Context {
        editor,
        scroll: Some(0),
        jobs: &mut Jobs::new(),
    };

    let options = commands::WriteAllOptions {
        force: false,
        write_scratch: false,
        auto_format: false,
        code_actions: false,
    };

    if let Err(e) = commands::typed::write_documents_impl(context, docs, options) {
        context.editor.set_error(format!("{}", e));
    }
}

/// A file you have moved away from is saved, the way leaving the terminal saves it: the
/// tab you left would otherwise be the only place your work lives.
fn save_on_leaving(editor: &mut Editor, doc: DocumentId) {
    if !editor.config().auto_save.focus_lost {
        return;
    }

    // A file being closed is gone by now, and one never written has nowhere to go.
    let Some(document) = editor.document(doc) else {
        return;
    };
    if !document.is_modified() || document.path().is_none() {
        return;
    }

    // Tidied the same way `:w` tidies, so a file is never written two different ways.
    let view = editor.get_synced_view_id(doc);
    let trim_final_newlines = editor.config().trim_final_newlines;
    let typing = commands::typed::typing(editor);
    let document = doc_mut!(editor, &doc);
    commands::typed::tidy_before_save(document, view, trim_final_newlines, typing);

    if let Err(err) = editor.save::<std::path::PathBuf>(doc, None, false) {
        editor.set_error(format!("Could not save: {err}"));
    }
}

pub(super) fn register_hooks(handlers: &Handlers) {
    register_hook!(move |event: &mut DocumentFocusLost<'_>| {
        save_on_leaving(event.editor, event.doc);
        Ok(())
    });

    let tx = handlers.auto_save.clone();
    register_hook!(move |event: &mut DocumentDidChange<'_>| {
        let config = event.doc.config.load();
        if config.auto_save.after_delay.enable {
            send_blocking(
                &tx,
                AutoSaveEvent::DocumentChanged {
                    save_after: config.auto_save.after_delay.timeout,
                    doc: event.doc.id(),
                },
            );
        }
        Ok(())
    });

    let tx = handlers.auto_save.clone();
    register_hook!(move |event: &mut OnModeSwitch<'_, '_>| {
        if event.old_mode == Mode::Insert {
            send_blocking(&tx, AutoSaveEvent::LeftInsertMode)
        }
        Ok(())
    });
}
