//! Every action the editor has, the keys that reach it, and the way to change them.
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use helix_view::{
    document::Mode,
    editor::ConfigEvent,
    graphics::{Modifier, Rect},
    input::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind},
    Editor,
};
use tui::{
    buffer::Buffer as Surface,
    widgets::{Block, Widget},
};

use crate::{
    compositor::{Component, Compositor, Context, Event, EventResult},
    keymap::KeyTrie,
    ui::{
        bindings::{self, Clash, Runs, Where, MODES, TABLES},
        confirm, context_menu,
    },
};

/// One line of the screen: an action, with the keys that run it if it has any.
struct Row {
    /// Where it works, and the table config.toml writes it under.
    place: Where,
    /// The "Where" column.
    scope: String,
    keys: Vec<KeyEvent>,
    /// The keys as they read: "Ctrl+S", "Space → h", or nothing at all.
    label: String,
    /// What it runs.
    runs: Runs,
    description: String,
    search: String,
    /// Whether this terminal sends the keys at all: a Cmd key in a terminal that keeps
    /// Cmd for itself is listed, so whoever looks for it learns why it is not here.
    reachable: bool,
    /// Whether the shortcut is as sid ships it, or one of yours.
    sids: bool,
}

impl Row {
    fn new(
        place: Where,
        keys: Vec<KeyEvent>,
        runs: Runs,
        description: String,
        also: &str,
        reachable: bool,
        sids: bool,
    ) -> Self {
        let description = description.split_whitespace().collect::<Vec<_>>().join(" ");
        let label = keys
            .iter()
            .map(|key| key_label(*key))
            .collect::<Vec<_>>()
            .join(" → ");
        // An action no key reaches yet is of no world in particular.
        let scope = if keys.is_empty() {
            "—".to_string()
        } else {
            place.label().to_string()
        };
        // What is typed looks at the keys and what they do, never at the column that
        // says where they work: the tabs are for that, and "modal" should find the way
        // into modal editing, not every shortcut that already lives there. The other
        // words an action answers to in the palette — "word wrap" for wrapping — are
        // searched here as well, and never shown.
        let search = format!("{label} {description} {} {also}", runs.text()).to_lowercase();
        Self {
            place,
            scope,
            keys,
            label,
            runs,
            description,
            search,
            reachable,
            sids,
        }
    }

    /// The "From" column: whose the shortcut is.
    fn source(&self) -> &'static str {
        if self.keys.is_empty() {
            "—"
        } else if self.sids {
            "sid"
        } else {
            "you"
        }
    }
}

/// What the tabs offer: everything, one of the two worlds a shortcut can be tied to, the
/// actions no key reaches here, and the shortcuts that are yours rather than sid's.
const SCOPES: &[&str] = &["All", "Insert", "Modal", "Unbound", "Yours"];

/// The keys being pressed for a shortcut, and what they would cost.
struct Capture {
    place: Where,
    runs: Runs,
    /// The action in words, for the box to name it.
    description: String,
    /// The keys it answers to now, which the new ones take the place of. Empty when no
    /// key reaches it yet, and then the new ones are simply added.
    was: Vec<KeyEvent>,
    keys: Vec<KeyEvent>,
    clash: Option<Clash>,
    /// With the keys another action's, whether the answer in focus is the swap: that
    /// action gets the keys this one had, instead of being left without any.
    swap: bool,
}

impl Capture {
    /// Whether the swap can be offered: the keys are another action's, and this one has
    /// keys of its own to hand over.
    fn can_swap(&self) -> bool {
        matches!(self.clash, Some(Clash::Taken { .. })) && !self.was.is_empty()
    }
}

/// A second click on the same row within this long is a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

pub struct Shortcuts {
    rows: Vec<Row>,
    query: String,
    /// A shortcut pressed to find what runs it, which narrows the list to the rows that
    /// begin with it.
    pressed: Option<Vec<KeyEvent>>,
    scope: usize,
    /// Which of the shown rows is in focus.
    cursor: usize,
    scroll: usize,
    page: usize,
    tabs: Vec<Rect>,
    /// Where each shown row was drawn, so a click can land on one.
    drawn: Vec<Rect>,
    /// The keymap as the editor is running it, so a clash is told from a free key.
    maps: HashMap<Mode, KeyTrie>,
    /// The keys sid ships with, so yours are told from its.
    sids: HashMap<Mode, KeyTrie>,
    enhanced: bool,
    capture: Option<Capture>,
    /// What just lost its shortcut, so the screen can put you on it.
    lost: Option<Runs>,
    /// What the last change did, said on the screen itself: the status line is not the
    /// place for it, because reading config.toml again writes its own line over it.
    said: Option<String>,
    /// What config.toml said before each change made here, and what the change was, the
    /// latest last: Ctrl+Z puts the file back as it was and says what it undid.
    undo: Vec<(Option<String>, String)>,
    /// The row last clicked and when, so a second click on it is a double click.
    last_click: Option<(usize, Instant)>,
}

pub(crate) fn key_label(key: KeyEvent) -> String {
    let mut parts = Vec::new();
    let mut code = key;
    code.modifiers = KeyModifiers::NONE;
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        parts.push("Ctrl".to_string());
    }
    if key.modifiers.contains(KeyModifiers::ALT) {
        parts.push("Alt".to_string());
    }
    if key.modifiers.contains(KeyModifiers::SUPER) {
        parts.push("Cmd".to_string());
    }
    let upper =
        matches!(key.code, KeyCode::Char(c) if c.is_ascii_uppercase()) && !key.modifiers.is_empty();
    if key.modifiers.contains(KeyModifiers::SHIFT) || upper {
        parts.push("Shift".to_string());
    }
    if upper {
        if let KeyCode::Char(c) = code.code {
            code.code = KeyCode::Char(c.to_ascii_lowercase());
        }
    }
    let label = match code.code {
        KeyCode::Char(' ') => "Space".to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Esc => "Esc".to_string(),
        _ => code.to_string(),
    };
    parts.push(label);
    parts.join("+")
}

/// A key as config.toml spells it, which is how the keymap is asked about it: what the
/// terminal sends for Shift and a letter is the same shortcut either way.
fn settled(key: KeyEvent) -> KeyEvent {
    let mut key = key;
    if let KeyCode::Char(c) = key.code {
        if key.modifiers.contains(KeyModifiers::SHIFT) && c.is_ascii_alphabetic() {
            key.code = KeyCode::Char(c.to_ascii_uppercase());
            key.modifiers.remove(KeyModifiers::SHIFT);
        }
    }
    key
}

/// The modifiers that make a key a shortcut rather than something typed.
const HELD: KeyModifiers = KeyModifiers::CONTROL
    .union(KeyModifiers::ALT)
    .union(KeyModifiers::SUPER);

/// Whether a key pressed on the list is a shortcut to look up rather than a letter for the
/// filter or a key of the screen's own: one held with Ctrl, Alt or Cmd, a function key,
/// or Shift with something that is not a letter.
fn is_shortcut(key: KeyEvent) -> bool {
    if key.modifiers.intersects(HELD) {
        return true;
    }
    match key.code {
        KeyCode::F(_) | KeyCode::Delete | KeyCode::Insert => true,
        KeyCode::Char(_) => false,
        _ => key.modifiers.contains(KeyModifiers::SHIFT),
    }
}

/// The keymaps a table of config.toml is laid under.
fn modes_of(table: &str) -> Vec<Mode> {
    match table {
        "normal" => vec![Mode::Normal],
        "select" => vec![Mode::Select],
        "insert" => vec![Mode::Insert],
        _ => MODES.to_vec(),
    }
}

/// The tables a shortcut of the place is dropped from when it is given back: one given
/// everywhere is taken from every table, or a line under a mode would keep it.
fn tables_of(place: Where) -> Vec<&'static str> {
    match place {
        Where::Anywhere => TABLES.to_vec(),
        other => other.tables(),
    }
}

impl Shortcuts {
    pub fn new(maps: &HashMap<Mode, KeyTrie>, enhanced: bool) -> Self {
        let mut screen = Self {
            rows: Vec::new(),
            query: String::new(),
            pressed: None,
            scope: 0,
            cursor: 0,
            scroll: 0,
            page: 1,
            tabs: Vec::new(),
            drawn: Vec::new(),
            maps: maps.clone(),
            sids: crate::config::default_keys(),
            enhanced,
            capture: None,
            lost: None,
            said: None,
            undo: Vec::new(),
            last_click: None,
        };
        screen.gather();
        screen
    }

    /// The same screen, already asking for the keys to give to `runs`: what Ctrl-k in the
    /// command palette opens, so a command found there is a shortcut away.
    pub fn giving(maps: &HashMap<Mode, KeyTrie>, enhanced: bool, runs: &Runs) -> Self {
        let mut screen = Self::new(maps, enhanced);
        // Its own line if a key already reaches it, and the one it waits on if none does.
        if screen.point_at(runs) {
            screen.start();
        }
        screen
    }

    /// Puts the focus on the row that runs `runs`, if one is shown.
    fn point_at(&mut self, runs: &Runs) -> bool {
        let shown = self.shown();
        let Some(at) = shown
            .iter()
            .position(|index| &self.rows[*index].runs == runs)
        else {
            return false;
        };
        self.cursor = at;
        self.follow(shown.len());
        true
    }

    /// Builds the list again from the keymap, which is what a change is seen through.
    fn gather(&mut self) {
        // One shortcut is one row, however many modes it is in: a key that works
        // everywhere reads as one line, not as three.
        let mut order: Vec<(Vec<KeyEvent>, Runs)> = Vec::new();
        let mut modes: HashMap<(Vec<KeyEvent>, Runs), Vec<Mode>> = HashMap::new();
        let mut says: HashMap<(Vec<KeyEvent>, Runs), String> = HashMap::new();
        for mode in MODES {
            let Some(map) = self.maps.get(&mode) else {
                continue;
            };
            let mut found = Vec::new();
            bindings::walk(map, &mut Vec::new(), &mut found);
            for (keys, runs, description) in found {
                let entry = (keys, runs);
                let seen = modes.entry(entry.clone()).or_insert_with(|| {
                    order.push(entry.clone());
                    Vec::new()
                });
                seen.push(mode);
                says.entry(entry).or_insert(description);
            }
        }

        let mut rows = Vec::new();
        let mut bound: HashSet<Runs> = HashSet::new();
        for entry in order {
            let (keys, runs) = entry.clone();
            let description = says.remove(&entry).unwrap_or_default();
            let in_modes = modes.remove(&entry).unwrap_or_default();
            bound.insert(runs.clone());
            // Two worlds, not three modes: typing, and the modal editing that normal and
            // select are two halves of. A shortcut in both worlds is one line.
            let mut places = Vec::new();
            if in_modes.len() == MODES.len() {
                places.push(Where::Anywhere);
            } else {
                if in_modes.contains(&Mode::Insert) {
                    places.push(Where::Insert);
                }
                let normal = in_modes.contains(&Mode::Normal);
                let select = in_modes.contains(&Mode::Select);
                if normal || select {
                    places.push(Where::Modal { normal, select });
                }
            }
            let reachable = bindings::reaches(&keys, self.enhanced);
            for place in places {
                let sids = self.as_sid_ships(place, &keys, &runs);
                rows.push(Row::new(
                    place,
                    keys.clone(),
                    runs.clone(),
                    description.clone(),
                    "",
                    reachable,
                    sids,
                ));
            }
        }
        rows.sort_by(|a, b| (&a.scope, &a.label).cmp(&(&b.scope, &b.label)));

        // And then everything the editor can do that no key reaches yet.
        let mut free: Vec<_> = bindings::catalogue()
            .into_iter()
            .filter(|(runs, _, _)| !bound.contains(runs))
            .collect();
        free.sort_by(|a, b| a.0.text().cmp(&b.0.text()));
        for (runs, description, also) in free {
            rows.push(Row::new(
                Where::Anywhere,
                Vec::new(),
                runs,
                description,
                &also,
                true,
                true,
            ));
        }

        self.rows = rows;
    }

    /// Whether the keys run the same thing in sid's own keymap, in every mode of the
    /// place: what tells a shortcut of sid's from one of yours.
    fn as_sid_ships(&self, place: Where, keys: &[KeyEvent], runs: &Runs) -> bool {
        place.modes().iter().all(|mode| {
            self.sids
                .get(mode)
                .and_then(|map| map.search(keys))
                .and_then(bindings::runs_of)
                .as_ref()
                == Some(runs)
        })
    }

    /// Each word typed in the filter, which every shown row has to have. Read once and
    /// handed around: there are hundreds of rows and every keystroke walks them all.
    fn words(&self) -> Vec<String> {
        self.query
            .to_lowercase()
            .split_whitespace()
            .map(ToString::to_string)
            .collect()
    }

    fn matches(&self, row: &Row, words: &[String]) -> bool {
        let scope = SCOPES[self.scope];
        let in_scope = match scope {
            "All" => true,
            "Unbound" => row.keys.is_empty() || !row.reachable,
            "Yours" => !row.sids && !row.keys.is_empty(),
            _ => row.scope == scope,
        };
        let by_keys = self
            .pressed
            .as_ref()
            .is_none_or(|pressed| row.keys.starts_with(pressed));
        in_scope && by_keys && words.iter().all(|word| row.search.contains(word))
    }

    /// The rows the screen is showing, as indices into all of them.
    fn shown(&self) -> Vec<usize> {
        let words = self.words();
        (0..self.rows.len())
            .filter(|index| self.matches(&self.rows[*index], &words))
            .collect()
    }

    /// The row in focus, if any is shown.
    fn focused(&self) -> Option<&Row> {
        self.shown()
            .get(self.cursor)
            .map(|index| &self.rows[*index])
    }

    fn walk_cursor(&mut self, delta: isize) {
        let count = self.shown().len();
        if count == 0 {
            self.cursor = 0;
            return;
        }
        self.cursor = (self.cursor as isize + delta).clamp(0, count as isize - 1) as usize;
        self.follow(count);
    }

    /// Keeps the row in focus on screen.
    fn follow(&mut self, count: usize) {
        let page = self.page.max(1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + page {
            self.scroll = self.cursor + 1 - page;
        }
        self.scroll = self.scroll.min(count.saturating_sub(page));
    }

    fn scroll(&mut self, delta: isize) {
        let count = self.shown().len();
        self.scroll = (self.scroll as isize + delta)
            .clamp(0, count.saturating_sub(self.page) as isize) as usize;
    }

    /// The list from the top again, after the filter changed.
    fn refilter(&mut self) {
        self.scroll = 0;
        self.cursor = 0;
    }

    /// Whether the keys are sid's own doing, which is what tells taking one away from
    /// giving one back.
    fn is_sids(&self, place: Where, keys: &[KeyEvent]) -> bool {
        place.modes().iter().any(|mode| {
            match self.sids.get(mode).and_then(|map| map.search(keys)) {
                Some(KeyTrie::MappableCommand(command)) => command.name() != "no_op",
                Some(KeyTrie::Sequence(_)) => true,
                _ => false,
            }
        })
    }

    /// The keys sid ships for what `runs`, with the mode each is in.
    fn sids_keys_for(&self, runs: &Runs) -> Vec<(Vec<KeyEvent>, Mode)> {
        let mut keys = Vec::new();
        for mode in MODES {
            let Some(map) = self.sids.get(&mode) else {
                continue;
            };
            let mut found = Vec::new();
            bindings::walk(map, &mut Vec::new(), &mut found);
            keys.extend(
                found
                    .into_iter()
                    .filter(|(_, what, _)| what == runs)
                    .map(|(keys, _, _)| (keys, mode)),
            );
        }
        keys
    }

    /// Starts giving the row in focus a shortcut.
    fn start(&mut self) {
        let Some(row) = self.focused() else {
            return;
        };
        // A shortcut that is given here is given everywhere, which is what a key means
        // in sid; one that already belongs to a world of its own stays in it.
        self.capture = Some(Capture {
            place: row.place,
            runs: row.runs.clone(),
            description: row.description.clone(),
            was: row.keys.clone(),
            keys: Vec::new(),
            clash: None,
            swap: false,
        });
    }

    /// What config.toml says right now, to be put back if the change is undone.
    fn snapshot() -> Option<String> {
        std::fs::read_to_string(helix_loader::config_file()).ok()
    }

    /// A change has been written: it is remembered for Ctrl+Z, the keymap is read again
    /// as the editor will read it, and the editor is asked to do the same.
    fn written(&mut self, editor: &mut Editor, before: Option<String>, said: String) {
        self.undo.push((before, said.clone()));
        self.reload();
        self.gather();
        refresh(editor);
        self.said = Some(said);
    }

    /// The keymap as the editor reads it from config.toml, which is what the screen shows
    /// after a change: what was done in memory is the same thing, but the file is the
    /// truth, and a line of yours under a mode that a change under `all` had to take
    /// away is only known there.
    fn reload(&mut self) {
        if let Ok(config) = crate::config::Config::load_default() {
            self.maps = config.keys;
        }
    }

    /// Gives the keys to the action, taking them from whatever had them.
    fn give(&mut self, editor: &mut Editor) {
        let Some(capture) = self.capture.take() else {
            return;
        };
        if capture.keys.is_empty() {
            return;
        }
        // Its own keys again: nothing changes, so nothing is written.
        if capture.keys == capture.was {
            self.said = Some(format!(
                "«{}» keeps {}",
                capture.description,
                key_path(&capture.keys)
            ));
            return;
        }
        // Nothing is written that the editor could not read back.
        let trie = match capture.runs.trie() {
            Ok(trie) => trie,
            Err(err) => {
                self.said = Some(format!("{err:#}"));
                editor.set_error(format!("{err:#}"));
                return;
            }
        };
        let taken = match &capture.clash {
            Some(Clash::Taken { what, runs }) => Some((what.clone(), runs.clone())),
            _ => None,
        };

        // What the keys used to run, so the screen can offer it another one.
        self.lost = taken.as_ref().map(|(_, runs)| runs.clone());

        let path = helix_loader::config_file();
        let before = Self::snapshot();
        if let Err(err) = bindings::write(&path, capture.place, &capture.keys, &capture.runs) {
            log::error!("Could not write the shortcut: {err:#}");
            self.said = Some(format!("Not written down: {err:#}"));
            editor.set_error(format!("Not written down: {err:#}"));
            return;
        }
        bindings::set(&mut self.maps, capture.place, &capture.keys, &trie);

        // Changing a shortcut changes it: the keys it answered to before stop reaching
        // it, or the line you edited would still be there beside the new one.
        let replaced = !capture.was.is_empty();
        if replaced {
            let sids = self.is_sids(capture.place, &capture.was);
            if let Err(err) = bindings::erase(&path, capture.place, &capture.was, sids) {
                log::error!("Could not take the old shortcut away: {err:#}");
                self.said = Some(format!("Half written down: {err:#}"));
                editor.set_error(format!("Half written down: {err:#}"));
            } else {
                bindings::unset(&mut self.maps, capture.place, &capture.was);
            }
        }

        // The swap: what lost the keys gets the ones this action had.
        let mut swapped = None;
        if let (true, Some((what, runs))) = (capture.swap && replaced, &taken) {
            match (
                bindings::write(&path, capture.place, &capture.was, runs),
                runs.trie(),
            ) {
                (Ok(()), Ok(trie)) => {
                    bindings::set(&mut self.maps, capture.place, &capture.was, &trie);
                    swapped = Some(what.clone());
                    self.lost = None;
                }
                (Err(err), _) | (_, Err(err)) => {
                    log::error!("Could not give the old keys to «{what}»: {err:#}");
                    editor.set_error(format!("«{what}» is left without keys: {err:#}"));
                }
            }
        }

        let keys = key_path(&capture.keys);
        let was = if replaced {
            format!(", and not {} any more", key_path(&capture.was))
        } else {
            String::new()
        };
        let mut said = format!("{keys} runs «{}» now{was}", capture.description);
        match (&taken, swapped) {
            (Some(_), Some(what)) => {
                said.push_str(&format!(
                    " · «{what}» runs on {} now",
                    key_path(&capture.was)
                ));
            }
            (Some((what, runs)), None) => {
                // What really happens to it: a key taken only while typing is still
                // its own while editing modally, and the other way round.
                let fate = self
                    .keeps(runs, &capture.keys, capture.place)
                    .unwrap_or("is left without it");
                said.push_str(&format!(" · «{what}» {fate}"));
            }
            (None, _) => {}
        }
        self.written(editor, before, said);
        self.point_at_lost();
    }

    /// Where an action still answers to keys just given to another, outside the place
    /// they were given in: while typing, or in modal editing. Nothing when it lost them.
    fn keeps(&self, runs: &Runs, keys: &[KeyEvent], place: Where) -> Option<&'static str> {
        let given = place.modes();
        let still: Vec<Mode> = MODES
            .into_iter()
            .filter(|mode| !given.contains(mode))
            .filter(|mode| {
                self.maps
                    .get(mode)
                    .and_then(|map| map.search(keys))
                    .and_then(bindings::runs_of)
                    .as_ref()
                    == Some(runs)
            })
            .collect();
        if still.is_empty() {
            None
        } else if still.contains(&Mode::Insert) {
            Some("keeps it while typing")
        } else {
            Some("keeps it in modal editing")
        }
    }

    /// Puts the focus on whatever just lost its keys, so it can be given others.
    fn point_at_lost(&mut self) {
        if let Some(lost) = self.lost.take() {
            self.point_at(&lost);
        }
    }

    /// Takes the shortcut in focus away.
    fn take_away(&mut self, editor: &mut Editor) {
        let Some(row) = self.focused() else {
            return;
        };
        if row.keys.is_empty() {
            return;
        }
        let (place, keys) = (row.place, row.keys.clone());
        let description = row.description.clone();
        let sids = self.is_sids(place, &keys);

        let before = Self::snapshot();
        if let Err(err) = bindings::erase(&helix_loader::config_file(), place, &keys, sids) {
            log::error!("Could not take the shortcut away: {err:#}");
            self.said = Some(format!("Not written down: {err:#}"));
            editor.set_error(format!("Not written down: {err:#}"));
            return;
        }
        bindings::unset(&mut self.maps, place, &keys);
        self.written(
            editor,
            before,
            format!("«{description}» has no {} now", key_path(&keys)),
        );
    }

    /// Gives the action in focus back to sid: its keys are what sid ships for it again,
    /// whatever you gave it and whatever you took away. By what it runs, not by the keys
    /// it has: one taken away has none, and it comes back all the same.
    fn give_back(&mut self, editor: &mut Editor) {
        let Some(row) = self.focused() else {
            return;
        };
        let (place, keys, runs) = (row.place, row.keys.clone(), row.runs.clone());
        let description = row.description.clone();

        // Your lines for the keys it has now, and for the keys sid gives it, wherever a
        // line of yours could shadow them.
        let mut lines: Vec<(Vec<KeyEvent>, Vec<&'static str>)> = Vec::new();
        if !keys.is_empty() {
            lines.push((keys, tables_of(place)));
        }
        let sids = self.sids_keys_for(&runs);
        for (keys, mode) in &sids {
            let table = match mode {
                Mode::Normal => "normal",
                Mode::Select => "select",
                Mode::Insert => "insert",
            };
            lines.push((keys.clone(), vec!["all", table]));
        }
        if lines.is_empty() {
            self.said = Some(format!(
                "«{description}» has no shortcut of sid's to go back to"
            ));
            return;
        }

        let before = Self::snapshot();
        let changed = match bindings::restore(&helix_loader::config_file(), &lines) {
            Ok(changed) => changed,
            Err(err) => {
                log::error!("Could not give the shortcut back: {err:#}");
                self.said = Some(format!("Not written down: {err:#}"));
                editor.set_error(format!("Not written down: {err:#}"));
                return;
            }
        };
        // In memory, each key goes back to what sid has for it, or to nothing.
        for (keys, tables) in &lines {
            for mode in tables.iter().flat_map(|table| modes_of(table)) {
                let sid = self
                    .sids
                    .get(&mode)
                    .and_then(|map| map.search(keys))
                    .cloned();
                let Some(map) = self.maps.get_mut(&mode) else {
                    continue;
                };
                match sid {
                    Some(trie) => bindings::set_in(map, keys, &trie),
                    None => bindings::unset_in(map, keys),
                }
            }
        }

        let its: Vec<String> = sids
            .iter()
            .map(|(keys, _)| keys.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .filter(|keys| bindings::reaches(keys, self.enhanced))
            .map(|keys| key_path(&keys))
            .collect();
        let said = match (changed, its.is_empty()) {
            (false, _) => format!("«{description}» is sid's own already"),
            (true, true) => format!("«{description}» has no shortcut, as sid ships it"),
            (true, false) => format!("{} is «{description}» again", its.join(", ")),
        };
        self.written(editor, before, said);
        self.point_at(&runs);
    }

    /// Gives every shortcut back to sid, once the question has been answered.
    fn give_all_back(&mut self, editor: &mut Editor) {
        let before = Self::snapshot();
        if let Err(err) = bindings::restore_all(&helix_loader::config_file()) {
            log::error!("Could not give the shortcuts back: {err:#}");
            self.said = Some(format!("Not written down: {err:#}"));
            editor.set_error(format!("Not written down: {err:#}"));
            return;
        }
        self.maps = self.sids.clone();
        self.written(editor, before, "Every shortcut is sid's own again".into());
    }

    /// Puts config.toml back as it was before the last change made here.
    fn undo(&mut self, editor: &mut Editor) {
        let Some((before, what)) = self.undo.pop() else {
            self.said = Some("Nothing to undo here".into());
            return;
        };
        let path = helix_loader::config_file();
        let put_back = match before {
            Some(text) => crate::ui::settings::write_atomically(&path, text),
            None => match std::fs::remove_file(&path) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                    Err(anyhow::Error::from(err))
                }
                _ => Ok(()),
            },
        };
        if let Err(err) = put_back {
            log::error!("Could not undo the change: {err:#}");
            self.said = Some(format!("Not undone: {err:#}"));
            editor.set_error(format!("Not undone: {err:#}"));
            return;
        }
        self.reload();
        self.gather();
        refresh(editor);
        self.said = Some(format!("Undone: {what}"));
    }

    /// One more key pressed towards a shortcut, and what it would cost. After keys that
    /// were refused, the next one starts over: pressing others means others, not more.
    fn press(&mut self, key: KeyEvent) {
        let Some(capture) = self.capture.as_mut() else {
            return;
        };
        if capture.clash.as_ref().is_some_and(Clash::refuses) {
            capture.keys.clear();
        }
        capture.keys.push(settled(key));
        capture.swap = false;
        self.weigh();
    }

    /// What the keys pressed so far stand to cost.
    fn weigh(&mut self) {
        let Some((place, keys)) = self
            .capture
            .as_ref()
            .map(|capture| (capture.place, capture.keys.clone()))
        else {
            return;
        };
        // A shortcut is never in its own way: pressing the keys it already has says
        // nothing, because nothing would change.
        let its_own = self
            .capture
            .as_ref()
            .is_some_and(|capture| capture.was == keys);
        let clash = (!its_own).then(|| bindings::clash(&self.maps, place, &keys, self.enhanced));
        if let Some(capture) = self.capture.as_mut() {
            capture.clash = clash.flatten();
            if !capture.can_swap() {
                capture.swap = false;
            }
        }
    }

    /// The shortcut that opens modal editing, as the footer says it: read from the keymap,
    /// so it is right whatever the key is set to. One that works while typing, which is
    /// where the way into modal editing is looked for, and that this terminal sends.
    fn modal_key(&self) -> Option<String> {
        let modal = Runs::One("normal_mode".into());
        self.rows
            .iter()
            .filter(|row| row.runs == modal && row.reachable && !row.keys.is_empty())
            .filter(|row| row.place.modes().contains(&Mode::Insert))
            .min_by_key(|row| {
                // The Cmd one on a Mac, the others elsewhere, and the shortest.
                let cmd = row
                    .keys
                    .iter()
                    .any(|key| key.modifiers.contains(KeyModifiers::SUPER));
                (
                    row.keys.len(),
                    cmd != cfg!(target_os = "macos"),
                    row.label.len(),
                )
            })
            .map(|row| row.label.clone())
    }

    /// The menu for the row in focus: everything that can be done to it besides changing
    /// its keys, which Enter does. It opens where the right button was pressed, or at
    /// the row when it was asked for by key.
    fn menu(&self, at: (u16, u16)) -> context_menu::ContextMenu {
        let mut entries = Vec::new();
        let on_screen = |run: fn(&mut Shortcuts, &mut Editor)| -> context_menu::Action {
            Box::new(move |compositor: &mut Compositor, cx: &mut Context| {
                if let Some(screen) = compositor.find::<Shortcuts>() {
                    run(screen, cx.editor);
                }
            })
        };
        if let Some(row) = self.focused() {
            entries.push(context_menu::Entry::new(
                "Change the keys",
                "Enter",
                on_screen(|screen, _| screen.start()),
            ));
            if !row.keys.is_empty() {
                entries.push(context_menu::Entry::new(
                    "Take the keys away",
                    "",
                    on_screen(|screen, editor| screen.take_away(editor)),
                ));
                entries.push(context_menu::Entry::new(
                    "Show what else has these keys",
                    "",
                    on_screen(|screen, _| {
                        if let Some(keys) = screen.focused().map(|row| row.keys.clone()) {
                            screen.pressed = Some(keys);
                            screen.scope = 0;
                            screen.refilter();
                        }
                    }),
                ));
            }
            entries.push(context_menu::Entry::new(
                "Give it back to sid",
                "",
                on_screen(|screen, editor| screen.give_back(editor)),
            ));
        }
        if !self.undo.is_empty() {
            entries.push(context_menu::Entry::new(
                "Undo the last change made here",
                "Ctrl+Z",
                on_screen(|screen, editor| screen.undo(editor)),
            ));
        }
        entries.push(context_menu::Entry::new(
            "Give every shortcut back to sid…",
            "",
            Box::new(|compositor, _| compositor.push(Box::new(ask_to_give_all_back()))),
        ));
        entries.push(context_menu::Entry::new(
            "Edit config.toml",
            "",
            Box::new(|compositor, cx| {
                // The screen closes: the file is edited where files are.
                compositor.pop();
                open_config_at_keys(cx.editor);
            }),
        ));
        context_menu::ContextMenu::new(at, entries)
    }

    /// Where the menu opens when it is asked for by key: on the row in focus.
    fn menu_here(&self) -> (u16, u16) {
        self.drawn
            .get(self.cursor.saturating_sub(self.scroll))
            .map(|row| (row.y, row.x + 2))
            .unwrap_or((0, 0))
    }

    fn open_menu(&self, at: (u16, u16)) -> EventResult {
        let menu = self.menu(at);
        EventResult::Consumed(Some(Box::new(move |compositor, _| {
            compositor.push(Box::new(menu));
        })))
    }
}

/// The question giving every shortcut back asks: it throws away every key you have ever
/// changed, and nothing brings them back.
fn ask_to_give_all_back() -> confirm::Confirm {
    confirm::Confirm::new(
        "Give every shortcut back to sid?",
        vec![
            "Every key you changed goes back to what sid ships with. Nothing brings them back."
                .into(),
        ],
        vec![
            confirm::Answer::new(
                "Give them back",
                Box::new(|_| {
                    crate::job::dispatch_blocking(|editor, compositor| {
                        if let Some(screen) = compositor.find::<Shortcuts>() {
                            screen.give_all_back(editor);
                        }
                    });
                }),
            )
            .destructive(),
            confirm::Answer::new("Keep them", Box::new(|_| {})),
        ],
    )
}

/// Opens config.toml in the editor, on its `[keys]` if it has one: for what this screen
/// cannot say, a chord under a mode of its own or a macro.
fn open_config_at_keys(editor: &mut Editor) {
    let path = helix_loader::config_file();
    if let Err(err) = editor.open(&path, helix_view::editor::Action::Replace) {
        editor.set_error(format!("Could not open {}: {err}", path.display()));
        return;
    }
    let (view, doc) = helix_view::current!(editor);
    let text = doc.text();
    let at = text
        .to_string()
        .find("[keys")
        .map(|byte| text.byte_to_char(byte))
        .unwrap_or_else(|| text.len_chars());
    doc.set_selection(view.id, helix_core::Selection::point(at));
    helix_view::align_view(doc, view, helix_view::Align::Center);
}

/// How a shortcut reads.
fn key_path(keys: &[KeyEvent]) -> String {
    keys.iter()
        .map(|key| key_label(*key))
        .collect::<Vec<_>>()
        .join(" → ")
}

/// Asks the editor to read config.toml again, so the new shortcut works at once. It says
/// nothing about it: the screen has already said what changed.
fn refresh(editor: &mut Editor) {
    if let Err(err) = editor.config_events.0.send(ConfigEvent::RefreshQuietly) {
        log::error!("The editor did not take the change: {err}");
    }
}

impl Component for Shortcuts {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let width = (area.width.saturating_sub(4)).min(120);
        let height = (area.height.saturating_sub(4)).min(32);
        let popup = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );
        let theme = &cx.editor.theme;
        let background = theme.get("ui.popup");
        surface.clear_with(popup, background);
        let block = Block::bordered().style(background);
        let inner = block.inner(popup);
        block.render(popup, surface);
        self.tabs.clear();
        self.drawn.clear();
        if inner.width < 12 || inner.height < 8 {
            return;
        }
        let x = inner.x + 2;
        let width = inner.width.saturating_sub(4) as usize;
        let title = theme.get("ui.text").add_modifier(Modifier::BOLD);
        let dim = theme.get("ui.text.inactive");
        let selected = theme.get("ui.menu.selected");
        surface.set_stringn(x, inner.y, "Keyboard shortcuts", width, title);
        // The build, so a report can say exactly which sid it came from.
        let version = format!("sid {}", crate::version::describe());
        let version_width = version.chars().count();
        if version_width + "Keyboard shortcuts".len() + 2 <= width {
            let version_x = x + (width - version_width) as u16;
            surface.set_stringn(version_x, inner.y, &version, version_width, dim);
        }
        let mut at = x;
        for (index, scope) in SCOPES.iter().enumerate() {
            let style = if index == self.scope {
                selected.patch(title)
            } else {
                dim
            };
            let room = inner.right().saturating_sub(at + 1) as usize;
            let (end, _) = surface.set_stringn(at, inner.y + 1, scope, room, style);
            self.tabs
                .push(Rect::new(at, inner.y + 1, end.saturating_sub(at), 1));
            at = end.saturating_add(3);
        }
        match &self.said {
            Some(said) => surface.set_stringn(x, inner.y + 2, said, width, title),
            None => surface.set_stringn(
                x,
                inner.y + 2,
                "Enter changes the keys · Shift+F10 or the right button: take them away, give them back, more · Ctrl+Z undoes",
                width,
                dim,
            ),
        };
        let filter = format!("Filter: {}▏", self.query);
        let (end, _) = surface.set_stringn(x, inner.y + 3, &filter, width, theme.get("ui.text"));
        if let Some(pressed) = &self.pressed {
            let pressed = format!("   Pressed: {}   (Backspace clears it)", key_path(pressed));
            let room = (x + width as u16).saturating_sub(end) as usize;
            surface.set_stringn(end, inner.y + 3, &pressed, room, title);
        }

        let shown = self.shown();
        self.page = inner.height.saturating_sub(7) as usize;
        self.cursor = self.cursor.min(shown.len().saturating_sub(1));
        self.follow(shown.len());
        let key_width = (width / 3).clamp(12, 36).min(width);
        let scope_width = 10usize.min(width.saturating_sub(key_width));
        let source_width = 5usize.min(width.saturating_sub(key_width + scope_width));
        let description_width = width.saturating_sub(key_width + scope_width + source_width);
        surface.set_stringn(x, inner.y + 5, "Shortcut", key_width, dim);
        surface.set_stringn(x + key_width as u16, inner.y + 5, "Where", scope_width, dim);
        surface.set_stringn(
            x + (key_width + scope_width) as u16,
            inner.y + 5,
            "From",
            source_width,
            dim,
        );
        surface.set_stringn(
            x + (key_width + scope_width + source_width) as u16,
            inner.y + 5,
            "Action",
            description_width,
            dim,
        );
        for (index, row) in shown
            .iter()
            .map(|index| &self.rows[*index])
            .enumerate()
            .skip(self.scroll)
            .take(self.page)
        {
            let y = inner.y + 6 + (index - self.scroll) as u16;
            let line = Rect::new(inner.x, y, inner.width, 1);
            let focused = index == self.cursor && self.capture.is_none();
            if focused {
                surface.set_style(line, selected);
            }
            self.drawn.push(line);
            let keys = if row.label.is_empty() {
                "—"
            } else {
                &row.label
            };
            // A key this terminal never sends is there to be seen, and seen as such.
            let key_style = match (focused, row.reachable) {
                (true, _) => selected.patch(title),
                (false, true) => title,
                (false, false) => dim,
            };
            let text_style = match (focused, row.reachable) {
                (true, _) => selected,
                (false, true) => theme.get("ui.text"),
                (false, false) => dim,
            };
            surface.set_string_truncated(
                x,
                y,
                keys,
                key_width.saturating_sub(2),
                |_| key_style,
                true,
                false,
            );
            surface.set_stringn(
                x + key_width as u16,
                y,
                &row.scope,
                scope_width.saturating_sub(1),
                if focused { selected } else { dim },
            );
            surface.set_stringn(
                x + (key_width + scope_width) as u16,
                y,
                row.source(),
                source_width.saturating_sub(1),
                if focused { selected } else { dim },
            );
            let description = if row.reachable {
                row.description.clone()
            } else {
                format!("{} · this terminal never sends it", row.description)
            };
            surface.set_string_truncated(
                x + (key_width + scope_width + source_width) as u16,
                y,
                &description,
                description_width,
                |_| text_style,
                true,
                false,
            );
        }
        let modal = self
            .modal_key()
            .map(|key| format!(" · Modal editing: {key}"))
            .unwrap_or_default();
        let footer = format!(
            "{} of {} · Type to filter, or press a shortcut to see what it runs · Tab: the list · Esc: close{modal}",
            shown.len(),
            self.rows.len()
        );
        surface.set_stringn(x, inner.bottom() - 1, &footer, width, dim);

        if self.capture.is_some() {
            self.render_capture(popup, surface, cx);
        }
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        if self.capture.is_some() {
            if let Event::Key(key) = event {
                self.capturing(*key, cx.editor);
            }
            return EventResult::Consumed(None);
        }
        match event {
            Event::Key(key) => {
                match (key.code, key.modifiers) {
                    (KeyCode::Esc | KeyCode::F(1), _) => {
                        return EventResult::Consumed(Some(Box::new(|compositor, _| {
                            compositor.pop();
                        })))
                    }
                    // What the right button opens, by key: the same menu.
                    (KeyCode::F(10), KeyModifiers::SHIFT) | (KeyCode::Menu, _) => {
                        return self.open_menu(self.menu_here());
                    }
                    (KeyCode::Tab, KeyModifiers::SHIFT) => {
                        self.scope = (self.scope + SCOPES.len() - 1) % SCOPES.len();
                        self.refilter();
                    }
                    (KeyCode::Tab, _) => {
                        self.scope = (self.scope + 1) % SCOPES.len();
                        self.refilter();
                    }
                    (KeyCode::Enter, _) => self.start(),
                    // Undo is undo wherever you are: here, of the changes made here.
                    (KeyCode::Char('z'), KeyModifiers::CONTROL | KeyModifiers::SUPER) => {
                        self.undo(cx.editor)
                    }
                    (KeyCode::Up, KeyModifiers::NONE) => self.walk_cursor(-1),
                    (KeyCode::Down, KeyModifiers::NONE) => self.walk_cursor(1),
                    (KeyCode::PageUp, KeyModifiers::NONE) => {
                        self.walk_cursor(-(self.page as isize))
                    }
                    (KeyCode::PageDown, KeyModifiers::NONE) => self.walk_cursor(self.page as isize),
                    (KeyCode::Home, KeyModifiers::NONE) => self.walk_cursor(isize::MIN / 2),
                    (KeyCode::End, KeyModifiers::NONE) => self.walk_cursor(isize::MAX / 2),
                    // Backspace takes back the last thing given to the filter: the
                    // shortcut pressed, and then the letters.
                    (KeyCode::Backspace, KeyModifiers::NONE) => {
                        if self.pressed.take().is_none() {
                            self.query.pop();
                        }
                        self.refilter();
                    }
                    (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                        self.query.push(c);
                        self.refilter();
                    }
                    // A shortcut pressed on the list is a question: what runs it?
                    _ if is_shortcut(*key) => {
                        self.pressed = Some(vec![settled(*key)]);
                        self.scope = 0;
                        self.refilter();
                    }
                    _ => {}
                }
            }
            Event::Mouse(event) => match event.kind {
                MouseEventKind::ScrollDown => self.scroll(3),
                MouseEventKind::ScrollUp => self.scroll(-3),
                MouseEventKind::Down(MouseButton::Left) => {
                    if let Some(index) = self.tabs.iter().position(|tab| {
                        event.row == tab.y && event.column >= tab.x && event.column < tab.right()
                    }) {
                        self.scope = index;
                        self.refilter();
                        return EventResult::Consumed(None);
                    }
                    if let Some(index) = self.drawn.iter().position(|row| {
                        event.row == row.y && event.column >= row.x && event.column < row.right()
                    }) {
                        let at = self.scroll + index;
                        self.cursor = at;
                        // A second click on the same row is Enter.
                        let again = self
                            .last_click
                            .is_some_and(|(row, when)| row == at && when.elapsed() <= DOUBLE_CLICK);
                        self.last_click = Some((at, Instant::now()));
                        if again {
                            self.last_click = None;
                            self.start();
                        }
                    }
                }
                MouseEventKind::Down(MouseButton::Right) => {
                    if let Some(index) = self.drawn.iter().position(|row| {
                        event.row == row.y && event.column >= row.x && event.column < row.right()
                    }) {
                        self.cursor = self.scroll + index;
                        return self.open_menu((event.row, event.column));
                    }
                }
                _ => {}
            },
            _ => {}
        }
        EventResult::Consumed(None)
    }
}

impl Shortcuts {
    /// The keys pressed while a shortcut is being given: Esc goes back, Enter takes the
    /// ones pressed, Backspace undoes the last of them, Tab turns to the other answer
    /// when there is one, and everything else is a key.
    fn capturing(&mut self, key: KeyEvent, editor: &mut Editor) {
        let empty = self
            .capture
            .as_ref()
            .is_some_and(|capture| capture.keys.is_empty());
        let refuses = self
            .capture
            .as_ref()
            .and_then(|capture| capture.clash.as_ref())
            .is_some_and(Clash::refuses);
        let can_swap = self.capture.as_ref().is_some_and(Capture::can_swap);
        match key.code {
            KeyCode::Esc => self.capture = None,
            // With nothing pressed yet, Enter and Backspace are shortcuts like any other;
            // refused keys stay refused, whatever Enter says.
            KeyCode::Enter if !empty && !refuses => self.give(editor),
            KeyCode::Enter if !empty => {}
            KeyCode::Backspace if !empty => {
                if let Some(capture) = self.capture.as_mut() {
                    capture.keys.pop();
                }
                self.weigh();
            }
            KeyCode::Tab if can_swap && key.modifiers.is_empty() => {
                if let Some(capture) = self.capture.as_mut() {
                    capture.swap = !capture.swap;
                }
            }
            _ => self.press(key),
        }
    }

    /// The box that asks for the keys and says what they would cost.
    fn render_capture(&self, popup: Rect, surface: &mut Surface, cx: &mut Context) {
        let Some(capture) = self.capture.as_ref() else {
            return;
        };
        let theme = &cx.editor.theme;
        let background = theme.get("ui.popup");
        let text = theme.get("ui.text");
        let dim = theme.get("ui.text.inactive");
        let bold = text.add_modifier(Modifier::BOLD);
        let warning = theme.get("warning");

        let keys = key_path(&capture.keys);
        let title = format!(
            "Shortcut for «{}» · {}",
            capture.description,
            capture.place.label()
        );
        // What it answers to now, so what the new keys take the place of is in sight.
        let pressed = match (keys.is_empty(), capture.was.is_empty()) {
            (true, true) => "Press the keys…".to_string(),
            (true, false) => format!("Press the keys…    now {}", key_path(&capture.was)),
            (false, true) => keys.clone(),
            (false, false) => format!("{keys}    instead of {}", key_path(&capture.was)),
        };
        let clash = capture.clash.as_ref().map(|clash| clash.says(&keys));
        let its_own = !capture.keys.is_empty() && capture.keys == capture.was;
        // The answers: with the keys another action's, two of them, and Tab turns from
        // one to the other.
        let mut answers = Vec::new();
        let hint = match (&capture.clash, capture.keys.is_empty()) {
            (_, true) => "Esc goes back".to_string(),
            _ if its_own => {
                "These are its keys already · Enter keeps them · Esc goes back".to_string()
            }
            (Some(clash), _) if clash.refuses() => "Press others · Esc goes back".to_string(),
            (Some(Clash::Taken { what, .. }), _) if capture.can_swap() => {
                let take = format!("Take it: «{what}» is left without it");
                let swap = format!("Swap: «{what}» gets {}", key_path(&capture.was));
                answers = vec![(take, !capture.swap), (swap, capture.swap)];
                "Enter takes the answer in focus · Tab turns to the other · Esc goes back"
                    .to_string()
            }
            (Some(Clash::Taken { .. }), _) => "Enter takes it · Esc goes back".to_string(),
            (Some(_), _) => "Enter gives it anyway · Esc goes back".to_string(),
            (None, _) => "Enter gives it · Backspace undoes a key · Esc goes back".to_string(),
        };

        let longest = [title.as_str(), pressed.as_str(), hint.as_str()]
            .into_iter()
            .chain(clash.as_deref())
            .chain(answers.iter().map(|(answer, _)| answer.as_str()))
            .map(|line| line.chars().count() + 2)
            .max()
            .unwrap_or(0);
        let width = (longest as u16 + 6).min(popup.width);
        let lines = 7 + clash.iter().count() as u16 + answers.len() as u16;
        let height = lines.min(popup.height);
        let box_area = Rect::new(
            popup.x + popup.width.saturating_sub(width) / 2,
            popup.y + popup.height.saturating_sub(height) / 2,
            width,
            height,
        );
        surface.clear_with(box_area, background);
        let block = Block::bordered().style(background);
        let inner = block.inner(box_area);
        block.render(box_area, surface);
        if inner.width < 4 {
            return;
        }

        let x = inner.x + 1;
        let room = inner.width.saturating_sub(2) as usize;
        surface.set_stringn(x, inner.y, &title, room, bold);
        surface.set_stringn(
            x,
            inner.y + 2,
            &pressed,
            room,
            if capture.keys.is_empty() { dim } else { bold },
        );
        let mut y = inner.y + 4;
        if let Some(clash) = &clash {
            surface.set_stringn(x, y, clash, room, warning);
            y += 1;
        }
        let selected = theme.get("ui.menu.selected");
        for (answer, focused) in &answers {
            let line = format!("{} {answer}", if *focused { "▸" } else { " " });
            surface.set_stringn(x, y, &line, room, if *focused { selected } else { text });
            y += 1;
        }
        surface.set_stringn(x, y, &hint, room, dim);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screen(toml: &str) -> Shortcuts {
        let trie: KeyTrie = toml::from_str(toml).unwrap();
        let maps = HashMap::from([(Mode::Insert, trie)]);
        Shortcuts::new(&maps, true)
    }

    fn key(name: &str) -> KeyEvent {
        name.parse::<KeyEvent>().unwrap()
    }

    #[test]
    fn the_list_has_the_shortcuts_and_everything_else_the_editor_can_do() {
        let mut screen = screen(
            r#"
            C-P = "keyboard_shortcuts"
            F3 = ["move_char_right", "move_char_left"]
            F4 = "@mihello<esc>"
            F5 = "no_op"
            [space]
            h = "file_history"
        "#,
        );
        assert!(screen
            .rows
            .iter()
            .any(|row| row.label == "Ctrl+Shift+p" && row.description.contains("shortcuts")));
        assert!(screen.rows.iter().any(|row| row.label == "Space → h"));
        assert!(screen
            .rows
            .iter()
            .any(|row| row.label == "F3" && row.description.contains(';')));
        assert!(screen.rows.iter().any(|row| row.label == "F4"));
        // A key left doing nothing is no shortcut.
        assert!(!screen
            .rows
            .iter()
            .any(|row| row.scope == "Insert" && row.label == "F5"));
        // Every action is there, whether a key reaches it or not.
        let unbound = screen
            .rows
            .iter()
            .find(|row| row.runs == Runs::One("duplicate_line".into()))
            .expect("every command is listed");
        assert!(unbound.keys.is_empty());
        assert!(screen
            .rows
            .iter()
            .any(|row| row.runs == Runs::One(":write".into())));

        // The tabs are the two worlds, what no key reaches, and what is yours: a panel's
        // own arrows are not shortcuts and are nowhere here.
        assert_eq!(SCOPES, ["All", "Insert", "Modal", "Unbound", "Yours"]);
        assert!(!screen.rows.iter().any(|row| row.scope == "Sidebar"));

        screen.scope = SCOPES.iter().position(|scope| *scope == "Unbound").unwrap();
        screen.query = "duplicate".into();
        let found = screen.shown();
        assert!(!found.is_empty());
        assert!(found
            .iter()
            .all(|index| screen.rows[*index].keys.is_empty()));
    }

    #[test]
    fn normal_and_select_are_one_world_and_typing_is_the_other() {
        let modal: KeyTrie = toml::from_str(r#"A-C-j = "duplicate_line""#).unwrap();
        let typing: KeyTrie = toml::from_str(r#"A-C-k = "select_all""#).unwrap();
        let maps = HashMap::from([
            (Mode::Normal, modal.clone()),
            (Mode::Select, modal),
            (Mode::Insert, typing),
        ]);
        let screen = Shortcuts::new(&maps, true);
        let at = |label: &str| {
            screen
                .rows
                .iter()
                .filter(|row| row.label == label)
                .collect::<Vec<_>>()
        };
        // Normal and select together are one line, not two.
        let modal = at("Ctrl+Alt+j");
        assert_eq!(modal.len(), 1);
        assert_eq!(modal[0].scope, "Modal");
        assert_eq!(
            modal[0].place,
            Where::Modal {
                normal: true,
                select: true
            }
        );
        let typing = at("Ctrl+Alt+k");
        assert_eq!(typing.len(), 1);
        assert_eq!(typing[0].scope, "Insert");
        assert_eq!(typing[0].place, Where::Insert);
    }

    #[test]
    fn a_shortcut_of_every_mode_is_one_line_and_not_three() {
        let trie: KeyTrie = toml::from_str(r#"C-A-j = "duplicate_line""#).unwrap();
        let maps = MODES.iter().map(|mode| (*mode, trie.clone())).collect();
        let screen = Shortcuts::new(&maps, true);
        let rows: Vec<_> = screen
            .rows
            .iter()
            .filter(|row| row.label == "Ctrl+Alt+j")
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].place, Where::Anywhere);
        assert_eq!(rows[0].scope, "Anywhere");
    }

    #[test]
    fn a_setting_can_be_given_a_key_whether_one_reaches_it_or_not() {
        // Alt-z already flips wrapping; the screen shows it by what it does.
        let trie: KeyTrie = toml::from_str(r#"A-z = ":toggle soft-wrap.enable""#).unwrap();
        let maps: HashMap<Mode, KeyTrie> = MODES.iter().map(|mode| (*mode, trie.clone())).collect();
        let screen = Shortcuts::new(&maps, true);
        let wrapping = Runs::One(":toggle-option soft-wrap.enable".into());
        let row = screen
            .rows
            .iter()
            .find(|row| row.runs == wrapping)
            .expect("the setting is listed");
        assert_eq!(row.label, "Alt+z");
        assert_eq!(row.description, "Wrap long lines, on or off");
        assert_eq!(
            screen
                .rows
                .iter()
                .filter(|row| row.runs == wrapping)
                .count(),
            1,
            "listed once, as a shortcut and not again as something waiting for one"
        );

        // And one no key reaches is waiting there to be given one.
        let blink = Runs::One(":toggle-option cursor-blink".into());
        let waiting = Shortcuts::giving(&maps, true, &blink);
        let capture = waiting.capture.as_ref().expect("it asks for the keys");
        assert_eq!(capture.runs, blink);
        assert!(capture.was.is_empty());
        assert!(
            capture.runs.trie().is_ok(),
            "and what it would write parses"
        );
    }

    #[test]
    fn a_setting_is_found_by_the_words_the_palette_knows_it_by() {
        let mut screen = screen("");
        let wrapping = Runs::One(":toggle-option soft-wrap.enable".into());
        screen.query = "word wrap".into();
        let found = screen.shown();
        assert!(found
            .iter()
            .any(|index| screen.rows[*index].runs == wrapping));
    }

    #[test]
    fn ctrl_k_in_the_palette_opens_the_screen_asking_for_the_keys() {
        let trie: KeyTrie = toml::from_str("").unwrap();
        let maps = MODES.iter().map(|mode| (*mode, trie.clone())).collect();
        let runs = Runs::One("duplicate_line".into());
        let screen = Shortcuts::giving(&maps, true, &runs);
        let capture = screen
            .capture
            .as_ref()
            .expect("it opens asking for the keys");
        assert_eq!(capture.runs, runs);
        // A shortcut given here is given everywhere.
        assert_eq!(capture.place, Where::Anywhere);
        assert!(capture.keys.is_empty());
    }

    #[test]
    fn a_shortcut_just_given_is_on_the_screen_before_config_toml_is_read_again() {
        let trie: KeyTrie = toml::from_str("").unwrap();
        let mut maps: HashMap<Mode, KeyTrie> =
            MODES.iter().map(|mode| (*mode, trie.clone())).collect();
        let mut screen = Shortcuts::new(&maps, true);
        let keys: Vec<KeyEvent> = vec![key("C-A-j")];
        let runs = Runs::One("duplicate_line".into());

        bindings::set(&mut maps, Where::Anywhere, &keys, &runs.trie().unwrap());
        screen.maps = maps;
        screen.gather();

        let row = screen
            .rows
            .iter()
            .find(|row| row.runs == runs)
            .expect("the command is listed");
        assert_eq!(row.label, "Ctrl+Alt+j");
        assert_eq!(row.place, Where::Anywhere);
        // And it is no longer waiting for a key.
        assert_eq!(screen.rows.iter().filter(|row| row.runs == runs).count(), 1);
    }

    #[test]
    fn changing_a_shortcut_knows_the_keys_it_takes_the_place_of() {
        let trie: KeyTrie =
            toml::from_str("C-A-j = \"duplicate_line\"\nC-A-k = \"select_all\"\n").unwrap();
        let maps = MODES.iter().map(|mode| (*mode, trie.clone())).collect();
        let mut screen = Shortcuts::new(&maps, true);
        let shown = screen.shown();
        screen.cursor = shown
            .iter()
            .position(|index| screen.rows[*index].label == "Ctrl+Alt+j")
            .expect("the shortcut is listed");
        screen.start();
        assert_eq!(
            screen.capture.as_ref().unwrap().was,
            vec![key("C-A-j")],
            "it knows what it is replacing"
        );

        // Its own keys are not in its own way.
        screen.press(key("C-A-j"));
        assert!(screen.capture.as_ref().unwrap().clash.is_none());

        // Another action's are, and the swap is on offer: Tab turns to it.
        screen.capture.as_mut().unwrap().keys.clear();
        screen.press(key("C-A-k"));
        let capture = screen.capture.as_ref().unwrap();
        assert!(matches!(capture.clash, Some(Clash::Taken { .. })));
        assert!(capture.can_swap());
        assert!(!capture.swap);
    }

    #[test]
    fn keys_that_are_refused_are_replaced_by_the_next_ones_pressed() {
        let trie: KeyTrie = toml::from_str("").unwrap();
        let maps = MODES.iter().map(|mode| (*mode, trie.clone())).collect();
        let runs = Runs::One("duplicate_line".into());
        let mut screen = Shortcuts::giving(&maps, true, &runs);

        // Enter is text while typing, so it is refused wherever a key means the same.
        screen.press(key("ret"));
        assert_eq!(screen.capture.as_ref().unwrap().clash, Some(Clash::Text));
        // The next key starts over instead of making a chord that begins with Enter.
        screen.press(key("F7"));
        let capture = screen.capture.as_ref().unwrap();
        assert_eq!(capture.keys, vec![key("F7")]);
        assert!(capture.clash.is_none());
    }

    #[test]
    fn the_keys_pressed_are_the_ones_config_toml_would_write() {
        let shift_s = KeyEvent {
            code: KeyCode::Char('s'),
            modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        };
        assert_eq!(settled(shift_s), key("C-S"));
        assert_eq!(settled(key("C-s")), key("C-s"));
    }

    #[test]
    fn a_shortcut_is_sids_or_yours_and_the_tab_tells_them_apart() {
        let mut maps = crate::config::default_keys();
        let mine: Vec<KeyEvent> = vec![key("C-A-j")];
        bindings::set(
            &mut maps,
            Where::Anywhere,
            &mine,
            &Runs::One("duplicate_line".into()).trie().unwrap(),
        );
        let mut screen = Shortcuts::new(&maps, true);
        let row = |screen: &Shortcuts, label: &str| {
            screen
                .rows
                .iter()
                .find(|row| row.label == label)
                .map(|row| row.source())
        };
        assert_eq!(row(&screen, "Ctrl+s"), Some("sid"));
        assert_eq!(row(&screen, "Ctrl+Alt+j"), Some("you"));

        screen.scope = SCOPES.iter().position(|scope| *scope == "Yours").unwrap();
        let shown = screen.shown();
        assert_eq!(shown.len(), 1);
        assert_eq!(screen.rows[shown[0]].label, "Ctrl+Alt+j");
    }

    #[test]
    fn a_key_the_terminal_never_sends_is_listed_and_said_so() {
        let maps = crate::config::default_keys();
        let screen = Shortcuts::new(&maps, false);
        let cmd_s = screen
            .rows
            .iter()
            .find(|row| row.label == "Cmd+s")
            .expect("a Cmd key is listed even where Cmd never arrives");
        assert!(!cmd_s.reachable);
        assert!(screen
            .rows
            .iter()
            .find(|row| row.label == "Ctrl+s")
            .is_some_and(|row| row.reachable));
        // Ctrl-Alt-m arrives as Alt-Enter there, and the footer says the one that works.
        assert_eq!(screen.modal_key().as_deref(), Some("Alt+Enter"));
        assert_eq!(
            Shortcuts::new(&maps, true).modal_key().as_deref(),
            Some("Alt+Enter"),
            "the shortest of the keys that reach"
        );
    }

    #[test]
    fn a_shortcut_pressed_on_the_list_shows_what_runs_it() {
        let maps = crate::config::default_keys();
        let mut screen = Shortcuts::new(&maps, true);
        screen.pressed = Some(vec![key("C-s")]);
        let shown = screen.shown();
        assert_eq!(shown.len(), 1);
        assert_eq!(screen.rows[shown[0]].runs, Runs::One(":write".into()));
        // Only a shortcut is one: letters go to the filter, arrows walk the list.
        assert!(is_shortcut(key("C-s")));
        assert!(is_shortcut(key("F7")));
        assert!(is_shortcut(key("S-del")));
        assert!(!is_shortcut(key("a")));
        assert!(!is_shortcut(key("A")));
        assert!(!is_shortcut(key("down")));
        assert!(!is_shortcut(key("backspace")));
    }
}
