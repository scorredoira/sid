//! The rows a tab lists, and what the tabs that list files on disk or in git share: which
//! directories are open, how a directory or a set of changed paths becomes rows, and how
//! a row of a file or a directory is drawn.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use helix_view::editor::FileExplorerConfig;
use helix_view::graphics::{Modifier, Rect, Style};
use helix_view::Theme;
use tui::buffer::Buffer as Surface;

use super::git::{Change, ChangedFile};
use super::list::List;
use crate::ui::{directory_entries_with, explorer_walker};

/// One line of a tab.
pub enum Row {
    /// A file or a directory, on disk or as a commit touched it.
    Entry(Entry),
    /// A commit, in the history or standing above its own files.
    Commit(CommitRow),
    /// A definition in the file being edited, in the outline under the tree.
    Symbol(SymbolRow),
}

pub struct SymbolRow {
    pub name: String,
    /// `function`, `struct`, `class`…: the tags query's word for it.
    pub kind: &'static str,
    /// How many definitions it sits inside, when the outline follows the file's order.
    pub depth: usize,
    /// Whether other definitions sit inside it, so it folds, and whether it is open.
    pub fold: Option<bool>,
    /// Where the definition starts and ends, in characters of the document.
    pub start: usize,
    pub end: usize,
    /// Where going to it lands: on its name.
    pub jump: usize,
}

pub struct Entry {
    pub path: PathBuf,
    pub name: String,
    pub is_dir: bool,
    pub depth: usize,
    pub change: Option<Change>,
    /// Git's two columns for a file in the working tree; a file on disk or in a commit
    /// has neither.
    pub staged: Option<Change>,
    pub unstaged: Option<Change>,
}

pub struct CommitRow {
    /// Its place in the tab's history, for the tab to find the commit by.
    pub index: usize,
    pub short: String,
    pub subject: String,
    pub time: i64,
    pub date: String,
    pub author: String,
    /// The columns the authors take in a wide list, so the subjects line up.
    pub author_width: usize,
    /// The opened commit's own row, over its files, which alone names its hash.
    pub head: bool,
}

impl Row {
    pub fn entry(&self) -> Option<&Entry> {
        match self {
            Row::Entry(entry) => Some(entry),
            Row::Commit(_) | Row::Symbol(_) => None,
        }
    }

    /// What typing in the list walks to: a file's or a directory's name, a definition's.
    pub fn name(&self) -> Option<&str> {
        match self {
            Row::Entry(entry) => Some(&entry.name),
            Row::Symbol(symbol) => Some(&symbol.name),
            Row::Commit(_) => None,
        }
    }

    pub fn path(&self) -> Option<&Path> {
        self.entry().map(|entry| entry.path.as_path())
    }

    /// The path of a directory row; a file's or a commit's is none.
    pub fn dir(&self) -> Option<&Path> {
        self.entry()
            .filter(|entry| entry.is_dir)
            .map(|entry| entry.path.as_path())
    }

    pub fn depth(&self) -> usize {
        match self {
            Row::Entry(entry) => entry.depth,
            Row::Symbol(symbol) => symbol.depth,
            Row::Commit(_) => 0,
        }
    }
}

/// Which directories of a tab are open. A tree on disk opens closed and remembers what was
/// opened; one of changes opens open and remembers what was closed.
#[derive(Clone)]
pub struct Folds {
    open_by_default: bool,
    toggled: HashSet<PathBuf>,
}

impl Folds {
    pub fn closed() -> Self {
        Self {
            open_by_default: false,
            toggled: HashSet::new(),
        }
    }

    pub fn opened() -> Self {
        Self {
            open_by_default: true,
            toggled: HashSet::new(),
        }
    }

    pub fn is_open(&self, path: &Path) -> bool {
        self.open_by_default != self.toggled.contains(path)
    }

    pub fn set(&mut self, path: PathBuf, open: bool) {
        if open == self.open_by_default {
            self.toggled.remove(&path);
        } else {
            self.toggled.insert(path);
        }
    }

    pub fn close_all(&mut self, dirs: impl Iterator<Item = PathBuf>) {
        if self.open_by_default {
            self.toggled.extend(dirs);
        } else {
            self.toggled.clear();
        }
    }

    /// Opens every directory between `root` and `path`, so `path` can be seen.
    pub fn open_ancestors(&mut self, root: &Path, path: &Path) {
        let mut dir = path.parent();
        while let Some(current) = dir {
            if current == root {
                break;
            }
            self.set(current.to_path_buf(), true);
            dir = current.parent();
        }
    }
}

/// What one directory held when it was last read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<(PathBuf, bool)>,
    /// The directory's modification time, and that of every directory a flattened entry
    /// passes through: a change to any of them is a change to what this listing shows.
    pub fingerprint: Vec<(PathBuf, Option<SystemTime>)>,
}

/// A directory as it was just read: what it holds, or why it could not be read, in which
/// case it is remembered as empty so it is not read again and again.
pub struct Listed {
    pub dir: PathBuf,
    pub listing: Listing,
    pub error: Option<String>,
}

/// The directories a tree holds listings for, by path.
pub type Listings = HashMap<PathBuf, Listing>;

/// Reads one directory. Runs off the main thread: the walk reads ignore files on the way.
pub fn list_dir(dir: &Path, config: &FileExplorerConfig) -> Listed {
    let (entries, error) = match directory_entries_with(dir, config, false) {
        Ok(entries) => (entries, None),
        Err(err) => (Vec::new(), Some(format!("{}: {}", dir.display(), err))),
    };
    let entries: Vec<(PathBuf, bool)> = entries
        .into_iter()
        .filter(|(path, _)| !path.ends_with(".."))
        .collect();
    let fingerprint = fingerprint(dir, &entries);
    Listed {
        dir: dir.to_path_buf(),
        listing: Listing {
            entries,
            fingerprint,
        },
        error,
    }
}

/// The directories whose modification time says whether `dir`'s listing still holds: the
/// directory, and the ones a flattened entry (`a/b/c` shown as one row) passes through.
fn fingerprint(dir: &Path, entries: &[(PathBuf, bool)]) -> Vec<(PathBuf, Option<SystemTime>)> {
    let mut dirs = vec![dir.to_path_buf()];
    for (path, is_dir) in entries {
        if !is_dir {
            continue;
        }
        let mut between = path.parent();
        while let Some(current) = between {
            if current == dir {
                break;
            }
            dirs.push(current.to_path_buf());
            between = current.parent();
        }
    }
    dirs.into_iter()
        .map(|dir| {
            let modified = std::fs::metadata(&dir)
                .and_then(|meta| meta.modified())
                .ok();
            (dir, modified)
        })
        .collect()
}

/// Whether a listing's fingerprint has moved on disk; read again, off the main thread.
pub fn fingerprint_moved(listing: &Listing) -> bool {
    listing.fingerprint.iter().any(|(dir, modified)| {
        let now = std::fs::metadata(dir).and_then(|meta| meta.modified()).ok();
        now != *modified
    })
}

/// Lays out `dir` from the listings held, recursing into the open directories; an open
/// directory with no listing yet is named in `missing` and its row left childless until
/// its listing lands.
pub fn list_cached(
    dir: &Path,
    depth: usize,
    folds: &Folds,
    listings: &Listings,
    rows: &mut Vec<Row>,
    missing: &mut Vec<PathBuf>,
) {
    let Some(listing) = listings.get(dir) else {
        missing.push(dir.to_path_buf());
        return;
    };
    for (path, is_dir) in &listing.entries {
        let name = path
            .strip_prefix(dir)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        let open = *is_dir && folds.is_open(path);
        rows.push(Row::Entry(Entry {
            path: path.clone(),
            name,
            is_dir: *is_dir,
            depth,
            change: None,
            staged: None,
            unstaged: None,
        }));
        if open {
            list_cached(path, depth + 1, folds, listings, rows, missing);
        }
    }
}

/// How many files one walk of the workspace reads for the filter before it stops.
pub const WALK_CAP: usize = 50_000;

/// The files of the whole workspace, read once for the filter to look through.
pub struct Walk {
    pub files: Vec<PathBuf>,
    /// Whether the walk stopped at `WALK_CAP` with more left unread.
    pub capped: bool,
}

/// Walks the whole workspace under the tree's own rules, off the main thread, stopping at
/// `WALK_CAP` files.
pub fn walk_workspace(root: &Path, config: &FileExplorerConfig) -> Walk {
    let mut files = Vec::new();
    let mut capped = false;
    for entry in explorer_walker(root, config).build() {
        let Ok(entry) = entry else {
            continue;
        };
        if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            continue;
        }
        if files.len() == WALK_CAP {
            capped = true;
            break;
        }
        files.push(entry.into_path());
    }
    Walk { files, capped }
}

/// Lays out the files of a walk whose path below `root` contains `filter`, case aside,
/// under their directories, all of them open, in the tree's order.
pub fn narrow_walk(root: &Path, files: &[PathBuf], filter: &str) -> Vec<Row> {
    let filter = filter.to_lowercase();
    let mut top = ChangeDir::default();
    for file in files {
        let Ok(relative) = file.strip_prefix(root) else {
            continue;
        };
        if !relative.to_string_lossy().to_lowercase().contains(&filter) {
            continue;
        }
        let mut parts: Vec<String> = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect();
        let Some(name) = parts.pop() else {
            continue;
        };
        let mut dir = &mut top;
        for part in parts {
            dir = dir.dirs.entry(part).or_default();
        }
        dir.files.insert(name, None);
    }
    let mut rows = Vec::new();
    list_change_dir(&top, root, 0, &Folds::opened(), &mut rows);
    rows
}

/// Keeps the files whose path below `root` contains `filter`, case aside, and the
/// directories on the way to them.
pub fn narrow(rows: Vec<Row>, root: &Path, filter: &str) -> Vec<Row> {
    let filter = filter.to_lowercase();
    let mut keep = vec![false; rows.len()];
    // The open directories above the row being looked at, one per depth.
    let mut above: Vec<usize> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let Some(entry) = row.entry() else {
            continue;
        };
        above.truncate(entry.depth);
        if entry.is_dir {
            above.push(index);
            continue;
        }
        let relative = entry.path.strip_prefix(root).unwrap_or(&entry.path);
        let matches = relative.to_string_lossy().to_lowercase().contains(&filter);
        if matches {
            keep[index] = true;
            for dir in &above {
                keep[*dir] = true;
            }
        }
    }
    rows.into_iter()
        .zip(keep)
        .filter(|(_, keep)| *keep)
        .map(|(row, _)| row)
        .collect()
}

/// Lists changed files under the directories that hold them, below `root`: a chain of
/// directories with nothing else in them is one row, and a path outside the root is left
/// out.
pub fn list_changed(root: &Path, files: &[ChangedFile], folds: &Folds, rows: &mut Vec<Row>) {
    let mut top = ChangeDir::default();
    for file in files {
        let Ok(relative) = file.path.strip_prefix(root) else {
            continue;
        };
        let mut parts: Vec<String> = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect();
        let Some(name) = parts.pop() else {
            continue;
        };
        let mut dir = &mut top;
        for part in parts {
            dir = dir.dirs.entry(part).or_default();
        }
        dir.files.insert(name, Some(file));
    }
    list_change_dir(&top, root, 0, folds, rows);
}

/// Lists changed files one per row, each with its whole path below `root`, in the order
/// git gave them; a path outside the root is left out.
pub fn list_paths(root: &Path, files: &[ChangedFile], rows: &mut Vec<Row>) {
    for file in files {
        let Ok(relative) = file.path.strip_prefix(root) else {
            continue;
        };
        rows.push(Row::Entry(Entry {
            path: file.path.clone(),
            name: relative.to_string_lossy().into_owned(),
            is_dir: false,
            depth: 0,
            change: Some(file.change),
            staged: file.staged,
            unstaged: file.unstaged,
        }));
    }
}

/// Files placed under the directories of their paths; a file with no change is one the
/// filter found on disk.
#[derive(Default)]
struct ChangeDir<'a> {
    dirs: BTreeMap<String, ChangeDir<'a>>,
    files: BTreeMap<String, Option<&'a ChangedFile>>,
}

fn list_change_dir(dir: &ChangeDir, path: &Path, depth: usize, folds: &Folds, rows: &mut Vec<Row>) {
    for (name, child) in &dir.dirs {
        let mut name = name.clone();
        let mut child = child;
        let mut child_path = path.join(&name);
        while child.files.is_empty() && child.dirs.len() == 1 {
            let Some((next_name, next)) = child.dirs.first_key_value() else {
                break;
            };
            name = format!("{name}/{next_name}");
            child_path = child_path.join(next_name);
            child = next;
        }
        let open = folds.is_open(&child_path);
        rows.push(Row::Entry(Entry {
            path: child_path.clone(),
            name,
            is_dir: true,
            depth,
            change: None,
            staged: None,
            unstaged: None,
        }));
        if open {
            list_change_dir(child, &child_path, depth + 1, folds, rows);
        }
    }
    for (name, file) in &dir.files {
        rows.push(Row::Entry(Entry {
            path: path.join(name),
            name: name.clone(),
            is_dir: false,
            depth,
            change: file.map(|file| file.change),
            staged: file.and_then(|file| file.staged),
            unstaged: file.and_then(|file| file.unstaged),
        }));
    }
}

/// Puts the cursor back on `path` after the rows were laid out again.
pub fn reselect(rows: &[Row], list: &mut List, path: Option<&Path>) {
    list.set_len(rows.len());
    let Some(path) = path else {
        return;
    };
    if let Some(index) = rows.iter().position(|row| row.path() == Some(path)) {
        list.select(index);
    }
}

/// Where a row is drawn and how: the line it takes, whether it is the selected one and
/// whether it is the file being edited.
pub struct RowPaint {
    pub line: Rect,
    pub selected: Option<Style>,
    pub current: bool,
}

pub fn change_style(change: Change, theme: &Theme) -> Style {
    match change {
        Change::Added => theme.get("diff.plus"),
        Change::Deleted => theme.get("diff.minus"),
        Change::Modified | Change::Renamed => theme.get("diff.delta"),
    }
}

/// What the file tree says, quietly, of a row git knows about: the letter of a changed
/// file, or that a directory holds one.
pub enum Mark<'a> {
    File(&'a ChangedFile),
    Dir,
}

/// Draws a file or a directory: its fold marker and name, indented by depth, and a changed
/// file's letter in the last columns, clear of the name. In the working tree the letter
/// sits in git's own column: the left one staged, the right one not.
pub fn draw_entry(
    surface: &mut Surface,
    paint: &RowPaint,
    entry: &Entry,
    open: bool,
    theme: &Theme,
) {
    draw_entry_marked(surface, paint, entry, open, theme, None);
}

/// The same, with what git says of a row of the tree in the tail: a changed file's
/// letter, dim, or a dot for a directory holding one, the name's column untouched. A
/// theme that has `ui.text.directory.changed` colours such a directory with it.
pub fn draw_entry_marked(
    surface: &mut Surface,
    paint: &RowPaint,
    entry: &Entry,
    open: bool,
    theme: &Theme,
    mark: Option<Mark>,
) {
    let text_style = theme.get("ui.text");
    let mut style = if entry.is_dir {
        match mark {
            Some(Mark::Dir) => theme
                .try_get_exact("ui.text.directory.changed")
                .unwrap_or_else(|| theme.get("ui.text.directory")),
            _ => theme.get("ui.text.directory"),
        }
    } else {
        entry
            .change
            .map_or(text_style, |change| change_style(change, theme))
    };
    if paint.current {
        style = style.add_modifier(Modifier::BOLD);
    }
    if let Some(selected) = paint.selected {
        style = style.patch(selected);
    }
    let marker = if !entry.is_dir {
        "  "
    } else if open {
        "▾ "
    } else {
        "▸ "
    };
    let indent = 1 + entry.depth * 2;
    let label = format!("{}{}", marker, entry.name);
    let x = paint.line.x + indent as u16;
    let two_columns = entry.staged.is_some() || entry.unstaged.is_some();
    let letter_room = match (entry.change.is_some() || mark.is_some(), two_columns) {
        (false, _) => 0,
        (true, false) => 3,
        (true, true) => 4,
    };
    let width = (paint.line.width as usize).saturating_sub(indent + letter_room);
    surface.set_string_truncated(x, paint.line.y, &label, width, |_| style, true, false);
    let right = paint.line.right().saturating_sub(2).max(paint.line.x);
    if let Some(mark) = mark {
        let mut dim = theme.get("ui.text.inactive");
        if let Some(selected) = paint.selected {
            dim = dim.patch(selected);
        }
        let letter = match mark {
            Mark::File(file) => file.change.letter(),
            Mark::Dir => "•",
        };
        surface.set_string(right, paint.line.y, letter, dim);
        return;
    }
    let Some(change) = entry.change else {
        return;
    };
    let letter_style = |change: Change| {
        let mut style = change_style(change, theme);
        if let Some(selected) = paint.selected {
            style = style.patch(selected);
        }
        style
    };
    let left = right.saturating_sub(1).max(paint.line.x);
    if entry.staged.is_none() && entry.unstaged.is_none() {
        surface.set_string(right, paint.line.y, change.letter(), letter_style(change));
        return;
    }
    if let Some(staged) = entry.staged {
        let style = letter_style(staged).add_modifier(Modifier::BOLD);
        surface.set_string(left, paint.line.y, staged.letter(), style);
    }
    if let Some(unstaged) = entry.unstaged {
        surface.set_string(
            right,
            paint.line.y,
            unstaged.letter(),
            letter_style(unstaged),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, is_dir: bool, depth: usize) -> Row {
        let path = PathBuf::from(path);
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        Row::Entry(Entry {
            path,
            name,
            is_dir,
            depth,
            change: None,
            staged: None,
            unstaged: None,
        })
    }

    fn names(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|row| row.entry().unwrap().name.clone())
            .collect()
    }

    #[test]
    fn the_cache_lays_out_open_directories_and_names_the_ones_it_lacks() {
        let root = PathBuf::from("/p");
        let mut listings = Listings::new();
        listings.insert(
            root.clone(),
            Listing {
                entries: vec![(root.join("src"), true), (root.join("README.md"), false)],
                fingerprint: vec![],
            },
        );
        let mut folds = Folds::closed();
        let mut rows = Vec::new();
        let mut missing = Vec::new();
        list_cached(&root, 0, &folds, &listings, &mut rows, &mut missing);
        assert_eq!(names(&rows), vec!["src", "README.md"]);
        assert!(missing.is_empty());

        // An open directory not read yet keeps its row and is asked for.
        folds.set(root.join("src"), true);
        let mut rows = Vec::new();
        list_cached(&root, 0, &folds, &listings, &mut rows, &mut missing);
        assert_eq!(names(&rows), vec!["src", "README.md"]);
        assert_eq!(missing, vec![root.join("src")]);

        listings.insert(
            root.join("src"),
            Listing {
                entries: vec![(root.join("src/main.rs"), false)],
                fingerprint: vec![],
            },
        );
        let mut rows = Vec::new();
        let mut missing = Vec::new();
        list_cached(&root, 0, &folds, &listings, &mut rows, &mut missing);
        assert_eq!(names(&rows), vec!["src", "main.rs", "README.md"]);
        assert_eq!(rows[1].depth(), 1);
        assert!(missing.is_empty());
    }

    #[test]
    fn a_filter_over_a_walk_lays_matches_out_as_a_tree() {
        let root = Path::new("/p");
        let files = vec![
            PathBuf::from("/p/README.md"),
            PathBuf::from("/p/src/deep/thing.rs"),
            PathBuf::from("/p/src/main.rs"),
            PathBuf::from("/p/docs/other.md"),
            PathBuf::from("/p/src/deep/Thin.rs"),
        ];
        let rows = narrow_walk(root, &files, "th");
        assert_eq!(
            names(&rows),
            vec!["docs", "other.md", "src/deep", "Thin.rs", "thing.rs"]
        );
        assert_eq!(rows[2].depth(), 0);
        assert_eq!(rows[3].depth(), 1);
        assert!(rows[2].dir().is_some());
        assert!(rows[3].entry().unwrap().change.is_none());

        assert!(narrow_walk(root, &files, "nothing").is_empty());
    }

    #[test]
    fn a_filter_keeps_matching_files_and_the_directories_above_them() {
        let root = Path::new("/p");
        let rows = vec![
            entry("/p/docs", true, 0),
            entry("/p/docs/guide.md", false, 1),
            entry("/p/src", true, 0),
            entry("/p/src/ui", true, 1),
            entry("/p/src/ui/Editor.rs", false, 2),
            entry("/p/src/main.rs", false, 1),
            entry("/p/README.md", false, 0),
        ];
        let narrowed = narrow(rows, root, "editor");
        assert_eq!(names(&narrowed), vec!["src", "ui", "Editor.rs"]);

        // The path counts, not only the name, and a directory with nothing in it goes.
        let rows = vec![
            entry("/p/docs", true, 0),
            entry("/p/src", true, 0),
            entry("/p/src/main.rs", false, 1),
        ];
        let narrowed = narrow(rows, root, "src/ma");
        assert_eq!(names(&narrowed), vec!["src", "main.rs"]);
    }
}
