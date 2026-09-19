use std::{
    sync::{
        atomic::{self, AtomicBool},
        Arc,
    },
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
    save_pending: Arc<AtomicBool>,
}

impl AutoSaveHandler {
    pub fn new() -> AutoSaveHandler {
        AutoSaveHandler {
            save_pending: Default::default(),
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
            Self::Event::DocumentChanged { save_after } => {
                Some(Instant::now() + Duration::from_millis(save_after))
            }
            Self::Event::LeftInsertMode => {
                if existing_debounce.is_some() {
                    // If the change happened more recently than the debounce, let the
                    // debounce run down before saving.
                    existing_debounce
                } else {
                    // Otherwise if there is a save pending, save immediately.
                    if self.save_pending.load(atomic::Ordering::Relaxed) {
                        self.finish_debounce();
                    }
                    None
                }
            }
        }
    }

    fn finish_debounce(&mut self) {
        let save_pending = self.save_pending.clone();
        job::dispatch_blocking(move |editor, _| {
            if editor.mode() == Mode::Insert && editor.config().default_mode != Mode::Insert {
                // Modal editing waits for the insert session to finish. In sid's
                // default mode it never finishes; write_all_impl commits the pending
                // edits to history before saving, keeping saved revisions accurate.
                save_pending.store(true, atomic::Ordering::Relaxed);
            } else {
                request_auto_save(editor);
                save_pending.store(false, atomic::Ordering::Relaxed);
            }
        })
    }
}

fn request_auto_save(editor: &mut Editor) {
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

    if let Err(e) = commands::typed::write_all_impl(context, options) {
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
    let document = doc_mut!(editor, &doc);
    if document.trim_trailing_whitespace() {
        commands::typed::trim_trailing_whitespace(document, view);
    }
    if trim_final_newlines {
        commands::typed::trim_final_newlines(document, view);
    }
    if document.insert_final_newline() {
        commands::typed::insert_final_newline(document, view);
    }

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
