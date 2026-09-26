//! Open files that something else changes on disk. The operating system says so (inotify,
//! FSEvents, ReadDirectoryChangesW, through `notify`), and the terminal regaining focus
//! looks again, for the file systems that never say, like most network ones.
//!
//! A buffer without unsaved changes reads the file again. One with changes is left as it
//! is and warned about, once for each change on disk: `:reload` takes the file, `:w!`
//! keeps the buffer.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use helix_event::register_hook;
use helix_view::events::{ConfigDidChange, DocumentDidClose, DocumentDidOpen};
use helix_view::handlers::Handlers;
use helix_view::{DocumentId, Editor};
use notify::{EventKind, RecursiveMode, Watcher as _};

/// How long a burst of events is let settle before the files are looked at: a save is
/// often a write, a rename and a change of attributes in a row.
const SETTLE: Duration = Duration::from_millis(100);

struct Watch {
    watcher: notify::RecommendedWatcher,
    /// The directories watched, each one level deep. An atomic save renames a new file
    /// over the old one, and a watch on the file itself would go with the old one.
    dirs: HashSet<PathBuf>,
}

static WATCH: Mutex<Option<Watch>> = Mutex::new(None);
/// The paths of the open files, which the watcher's thread filters its events by. A lock
/// of its own: the watcher waits on its thread to add a directory, and that thread may be
/// waiting on this one.
static FILES: LazyLock<Mutex<HashSet<PathBuf>>> = LazyLock::new(Mutex::default);
/// A look at the files is already on its way.
static PENDING: AtomicBool = AtomicBool::new(false);
/// The change on disk each buffer with unsaved changes was last warned about.
static WARNED: LazyLock<Mutex<HashMap<DocumentId, SystemTime>>> = LazyLock::new(Mutex::default);

/// What these locks hold stays whole through a panic elsewhere, so a poisoned one is still
/// worth reading.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(super) fn register_hooks(_handlers: &Handlers) {
    register_hook!(move |event: &mut DocumentDidOpen<'_>| {
        sync(event.editor);
        Ok(())
    });
    register_hook!(move |event: &mut DocumentDidClose<'_>| {
        lock(&WARNED).remove(&event.doc.id());
        sync(event.editor);
        Ok(())
    });
    register_hook!(move |event: &mut ConfigDidChange<'_>| {
        if event.old.auto_reload != event.new.auto_reload {
            sync(event.editor);
        }
        Ok(())
    });
}

/// Watches the directories of the open files, and only those; none with `auto-reload` off.
pub(crate) fn sync(editor: &Editor) {
    let files: HashSet<PathBuf> = if editor.config().auto_reload {
        editor
            .documents()
            .filter_map(|doc| doc.path().map(Path::to_path_buf))
            .collect()
    } else {
        HashSet::new()
    };
    let dirs: HashSet<PathBuf> = files
        .iter()
        .filter_map(|file| file.parent().map(Path::to_path_buf))
        .collect();
    *lock(&FILES) = files;

    let mut watch = lock(&WATCH);
    if watch.is_none() {
        if dirs.is_empty() {
            return;
        }
        match start() {
            Ok(started) => *watch = Some(started),
            Err(err) => {
                log::warn!("cannot watch the open files for changes on disk: {err}");
                return;
            }
        }
    }
    let Some(watch) = watch.as_mut() else {
        return;
    };
    let gone: Vec<PathBuf> = watch.dirs.difference(&dirs).cloned().collect();
    for dir in gone {
        // A directory removed from disk took its watch with it.
        let _ = watch.watcher.unwatch(&dir);
        watch.dirs.remove(&dir);
    }
    for dir in dirs {
        if watch.dirs.contains(&dir) {
            continue;
        }
        match watch.watcher.watch(&dir, RecursiveMode::NonRecursive) {
            Ok(()) => {
                watch.dirs.insert(dir);
            }
            Err(err) => log::warn!("cannot watch {} for changes: {err}", dir.display()),
        }
    }
}

fn start() -> notify::Result<Watch> {
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|err| notify::Error::generic(&err.to_string()))?;
    let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let Ok(event) = event else {
            return;
        };
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        let files = lock(&FILES);
        if !event.paths.iter().any(|path| files.contains(path)) {
            return;
        }
        drop(files);
        if PENDING.swap(true, Ordering::AcqRel) {
            return;
        }
        runtime.spawn(async {
            tokio::time::sleep(SETTLE).await;
            crate::job::dispatch(|editor, _| {
                PENDING.store(false, Ordering::Release);
                look(editor);
            })
            .await;
        });
    })?;
    Ok(Watch {
        watcher,
        dirs: HashSet::new(),
    })
}

/// Looks off the main thread at the open files for one written since the buffer last
/// read or saved it, and lands what it finds.
pub(crate) fn look(editor: &mut Editor) {
    // A save under another name moved a buffer to a directory not yet watched.
    sync(editor);
    if !editor.config().auto_reload {
        return;
    }
    let docs: Vec<_> = editor
        .documents()
        .filter_map(|doc| {
            let path = doc.path()?.to_path_buf();
            Some((doc.id(), path, doc.last_saved_time(), doc.text().clone()))
        })
        .collect();
    if docs.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let found = tokio::task::spawn_blocking(move || {
            docs.into_iter()
                .filter_map(|(id, path, saved, text)| {
                    let written = std::fs::metadata(&path).ok()?.modified().ok()?;
                    if written <= saved {
                        return None;
                    }
                    // A save of this very buffer still on its way reads the same.
                    let same = std::fs::File::open(&path)
                        .ok()
                        .and_then(|mut file| {
                            helix_view::document::from_reader(&mut file, None).ok()
                        })
                        .is_some_and(|(on_disk, ..)| on_disk == text);
                    Some((id, written, same))
                })
                .collect::<Vec<_>>()
        })
        .await;
        match found {
            Ok(found) if !found.is_empty() => {
                crate::job::dispatch(move |editor, _| landed(editor, found)).await;
            }
            Ok(_) => {}
            Err(err) => log::error!("looking for files changed on disk stopped: {err}"),
        }
    });
}

fn landed(editor: &mut Editor, found: Vec<(DocumentId, SystemTime, bool)>) {
    if !editor.config().auto_reload {
        return;
    }
    for (id, written, same) in found {
        let Some(doc) = editor.document_mut(id) else {
            continue;
        };
        // Saved or read again while the look was out.
        if written <= doc.last_saved_time() {
            continue;
        }
        let Some(path) = doc.path().map(Path::to_path_buf) else {
            continue;
        };
        if same {
            doc.set_last_saved_time(written);
            lock(&WARNED).remove(&id);
        } else if !doc.is_modified() {
            lock(&WARNED).remove(&id);
            reload_document(editor, &path);
        } else if lock(&WARNED).insert(id, written) != Some(written) {
            let name = doc.display_name().into_owned();
            editor.set_warning(format!(
                "{name} changed on disk: :reload takes it and drops your changes, :w! keeps yours"
            ));
        }
    }
}

/// Reads the document open on `path` again from disk, the way `:reload` does, keeping
/// every view on it where it was.
pub(crate) fn reload_document(editor: &mut Editor, path: &Path) {
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
