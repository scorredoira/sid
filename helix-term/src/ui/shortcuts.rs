//! Every action the editor has, the keys that reach it, and the way to change them.
use std::collections::{HashMap, HashSet};

use helix_view::{
    document::Mode,
    editor::ConfigEvent,
    graphics::{Modifier, Rect},
    input::{KeyCode, KeyEvent, KeyModifiers, MouseEventKind},
};
use tui::{
    buffer::Buffer as Surface,
    widgets::{Block, Widget},
};

use crate::{
    compositor::{Component, Context, Event, EventResult},
    keymap::KeyTrie,
    ui::bindings::{self, Clash, Runs, Where, MODES},
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
}

impl Row {
    fn new(place: Where, keys: Vec<KeyEvent>, runs: Runs, description: String) -> Self {
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
        // What is typed looks at the keys and at what they do, never at the column that
        // says where they work: the tabs are for that, and "modal" should find the way
        // into modal editing, not every shortcut that already lives there.
        let search = format!("{label} {description} {}", runs.text()).to_lowercase();
        Self {
            place,
            scope,
            keys,
            label,
            runs,
            description,
            search,
        }
    }
}

/// What the tabs offer: everything, one of the two worlds a shortcut can be tied to, or
/// the actions no key reaches yet.
const SCOPES: &[&str] = &["All", "Insert", "Modal", "Unbound"];

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
}

pub struct Shortcuts {
    rows: Vec<Row>,
    query: String,
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
    /// Whether giving every shortcut back has been asked for once. It throws away every
    /// key you ever changed, so it is asked for twice.
    armed: bool,
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

impl Shortcuts {
    pub fn new(maps: &HashMap<Mode, KeyTrie>, enhanced: bool) -> Self {
        let mut screen = Self {
            rows: Vec::new(),
            query: String::new(),
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
            armed: false,
        };
        screen.gather();
        screen
    }

    /// The same screen, already asking for the keys to give to `runs`: what Ctrl-k in the
    /// command palette opens, so a command found there is a shortcut away.
    pub fn giving(maps: &HashMap<Mode, KeyTrie>, enhanced: bool, runs: &Runs) -> Self {
        let mut screen = Self::new(maps, enhanced);
        let shown = screen.shown();
        // Its own line if a key already reaches it, and the one it waits on if none does.
        if let Some(at) = shown
            .iter()
            .position(|index| &screen.rows[*index].runs == runs)
        {
            screen.cursor = at;
            let count = shown.len();
            screen.follow(count);
            screen.start();
        }
        screen
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
            bindings::walk(map, &mut Vec::new(), self.enhanced, &mut found);
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
            for place in places {
                rows.push(Row::new(
                    place,
                    keys.clone(),
                    runs.clone(),
                    description.clone(),
                ));
            }
        }
        rows.sort_by(|a, b| (&a.scope, &a.label).cmp(&(&b.scope, &b.label)));

        // And then everything the editor can do that no key reaches yet.
        let mut free: Vec<_> = bindings::catalogue()
            .into_iter()
            .filter(|(runs, _)| !bound.contains(runs))
            .collect();
        free.sort_by(|a, b| a.0.text().cmp(&b.0.text()));
        for (runs, description) in free {
            rows.push(Row::new(Where::Anywhere, Vec::new(), runs, description));
        }

        self.rows = rows;
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
            "Unbound" => row.keys.is_empty(),
            _ => row.scope == scope,
        };
        in_scope && words.iter().all(|word| row.search.contains(word))
    }

    /// The rows the screen is showing, as indices into all of them.
    fn shown(&self) -> Vec<usize> {
        let words = self.words();
        (0..self.rows.len())
            .filter(|index| self.matches(&self.rows[*index], &words))
            .collect()
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

    /// Starts giving the row in focus a shortcut.
    fn start(&mut self) {
        let Some(&index) = self.shown().get(self.cursor) else {
            return;
        };
        let row = &self.rows[index];
        // A shortcut that is given here is given everywhere, which is what a key means
        // in sid; one that already belongs to a world of its own stays in it.
        self.capture = Some(Capture {
            place: row.place,
            runs: row.runs.clone(),
            description: row.description.clone(),
            was: row.keys.clone(),
            keys: Vec::new(),
            clash: None,
        });
    }

    /// Gives the keys to the action, taking them from whatever had them.
    fn give(&mut self, cx: &mut Context) {
        let Some(capture) = self.capture.take() else {
            return;
        };
        if capture.keys.is_empty() {
            return;
        }
        let taken = match &capture.clash {
            Some(Clash::Taken { what }) => Some(what.clone()),
            _ => None,
        };

        // What the keys used to run, so the screen can offer it another one.
        self.lost = capture.place.modes().iter().find_map(|mode| {
            match self
                .maps
                .get(mode)
                .and_then(|map| map.search(&capture.keys))
            {
                Some(KeyTrie::MappableCommand(command)) if command.name() != "no_op" => {
                    Some(Runs::of(command))
                }
                _ => None,
            }
        });

        let path = helix_loader::config_file();
        if let Err(err) = bindings::write(&path, capture.place, &capture.keys, &capture.runs) {
            log::error!("Could not write the shortcut: {err:#}");
            self.said = Some(format!("Not written down: {err:#}"));
            cx.editor.set_error(format!("Not written down: {err:#}"));
            return;
        }
        match capture.runs.trie() {
            Ok(trie) => bindings::set(&mut self.maps, capture.place, &capture.keys, &trie),
            Err(err) => {
                self.said = Some(format!("{err:#}"));
                cx.editor.set_error(format!("{err:#}"));
                return;
            }
        }
        // Changing a shortcut changes it: the keys it answered to before stop reaching
        // it, or the line you edited would still be there beside the new one.
        let replaced = !capture.was.is_empty() && capture.was != capture.keys;
        if replaced {
            let sids = self.is_sids(capture.place, &capture.was);
            if let Err(err) = bindings::erase(&path, capture.place, &capture.was, sids) {
                log::error!("Could not take the old shortcut away: {err:#}");
                self.said = Some(format!("Half written down: {err:#}"));
                cx.editor.set_error(format!("Half written down: {err:#}"));
            } else {
                bindings::unset(&mut self.maps, capture.place, &capture.was);
            }
        }

        self.gather();
        refresh(cx);

        let keys = key_path(&capture.keys);
        let was = if replaced {
            format!(", and not {} any more", key_path(&capture.was))
        } else {
            String::new()
        };
        self.said = Some(match taken {
            Some(what) => format!(
                "{keys} runs «{}» now{was} · «{what}» is left without it",
                capture.description
            ),
            None => format!("{keys} runs «{}»{was}", capture.description),
        });
        self.point_at_lost();
    }

    /// Puts the focus on whatever just lost its keys, so it can be given others.
    fn point_at_lost(&mut self) {
        let Some(lost) = self.lost.take() else { return };
        let shown = self.shown();
        if let Some(at) = shown
            .iter()
            .position(|index| self.rows[*index].runs == lost)
        {
            self.cursor = at;
            let count = shown.len();
            self.follow(count);
        }
    }

    /// Takes the shortcut in focus away.
    fn take_away(&mut self, cx: &mut Context) {
        let Some(&index) = self.shown().get(self.cursor) else {
            return;
        };
        let row = &self.rows[index];
        if row.keys.is_empty() {
            return;
        }
        let (place, keys) = (row.place, row.keys.clone());
        let description = row.description.clone();
        let sids = self.is_sids(place, &keys);

        if let Err(err) = bindings::erase(&helix_loader::config_file(), place, &keys, sids) {
            log::error!("Could not take the shortcut away: {err:#}");
            self.said = Some(format!("Not written down: {err:#}"));
            cx.editor.set_error(format!("Not written down: {err:#}"));
            return;
        }
        bindings::unset(&mut self.maps, place, &keys);
        self.gather();
        refresh(cx);
        self.said = Some(format!("«{description}» has no shortcut now"));
    }

    /// Gives the keys in focus back to sid.
    fn give_back(&mut self, cx: &mut Context) {
        let Some(&index) = self.shown().get(self.cursor) else {
            return;
        };
        let row = &self.rows[index];
        if row.keys.is_empty() {
            return;
        }
        let (place, keys) = (row.place, row.keys.clone());

        if let Err(err) = bindings::restore(&helix_loader::config_file(), place, &keys) {
            log::error!("Could not give the shortcut back: {err:#}");
            self.said = Some(format!("Not written down: {err:#}"));
            cx.editor.set_error(format!("Not written down: {err:#}"));
            return;
        }
        let mut given = None;
        for mode in place.modes() {
            if let Some(trie) = self.sids.get(&mode).and_then(|map| map.search(&keys)) {
                given = Some(trie.clone());
                break;
            }
        }
        match &given {
            Some(trie) => bindings::set(&mut self.maps, place, &keys, trie),
            None => bindings::unset(&mut self.maps, place, &keys),
        }
        self.gather();
        refresh(cx);
        let keys = key_path(&keys);
        self.said = Some(match given {
            Some(trie) => format!("{keys} is «{}» again", bindings::describes(&trie)),
            None => format!("{keys} is free again"),
        });
    }

    /// Gives every shortcut back to sid. Asked for twice: it throws away every key you
    /// have ever changed, and nothing brings them back.
    fn give_all_back(&mut self, cx: &mut Context) {
        if !self.armed {
            self.armed = true;
            self.said =
                Some("This gives every shortcut back to sid. Ctrl+Alt+R again to do it.".into());
            return;
        }
        self.armed = false;

        if let Err(err) = bindings::restore_all(&helix_loader::config_file()) {
            log::error!("Could not give the shortcuts back: {err:#}");
            self.said = Some(format!("Not written down: {err:#}"));
            cx.editor.set_error(format!("Not written down: {err:#}"));
            return;
        }
        self.maps = self.sids.clone();
        self.gather();
        refresh(cx);
        self.said = Some("Every shortcut is sid's own again".into());
    }

    /// One more key pressed towards a shortcut, and what it would cost.
    fn press(&mut self, key: KeyEvent) {
        let Some(capture) = self.capture.as_mut() else {
            return;
        };
        capture.keys.push(settled(key));
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
        }
    }
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
fn refresh(cx: &mut Context) {
    if let Err(err) = cx.editor.config_events.0.send(ConfigEvent::RefreshQuietly) {
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
                "Enter to change it · Del to take it away · Ctrl+R to restore it · Ctrl+Alt+R to restore them all",
                width,
                dim,
            ),
        };
        let filter = format!("Filter: {}▏", self.query);
        surface.set_stringn(x, inner.y + 3, &filter, width, theme.get("ui.text"));

        let shown = self.shown();
        self.page = inner.height.saturating_sub(7) as usize;
        self.cursor = self.cursor.min(shown.len().saturating_sub(1));
        self.follow(shown.len());
        let key_width = (width / 3).clamp(12, 36).min(width);
        let scope_width = 10usize.min(width.saturating_sub(key_width));
        let description_width = width.saturating_sub(key_width + scope_width);
        surface.set_stringn(x, inner.y + 5, "Shortcut", key_width, dim);
        surface.set_stringn(x + key_width as u16, inner.y + 5, "Where", scope_width, dim);
        surface.set_stringn(
            x + (key_width + scope_width) as u16,
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
            let key_style = if focused {
                selected.patch(title)
            } else {
                title
            };
            let text_style = if focused {
                selected
            } else {
                theme.get("ui.text")
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
            surface.set_string_truncated(
                x + (key_width + scope_width) as u16,
                y,
                &row.description,
                description_width,
                |_| text_style,
                true,
                false,
            );
        }
        let footer = format!(
            "{} of {} · Type to filter · Tab: the list · ↑↓: move · Esc: close · Modal editing: Ctrl+Shift+P",
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
                self.capturing(*key, cx);
            }
            return EventResult::Consumed(None);
        }
        match event {
            Event::Key(key) => {
                // Any other key answers "no" to giving every shortcut back.
                let asking_again = key.code == KeyCode::Char('r')
                    && key.modifiers == KeyModifiers::CONTROL | KeyModifiers::ALT;
                if !asking_again {
                    self.armed = false;
                }
                match (key.code, key.modifiers) {
                    (KeyCode::Esc | KeyCode::F(1), _) => {
                        return EventResult::Consumed(Some(Box::new(|compositor, _| {
                            compositor.pop();
                        })))
                    }
                    (KeyCode::Tab, _) => {
                        self.scope = (self.scope + 1) % SCOPES.len();
                        self.scroll = 0;
                        self.cursor = 0;
                    }
                    (KeyCode::Enter, _) => self.start(),
                    (KeyCode::Delete, _) => self.take_away(cx),
                    (KeyCode::Char('r'), modifiers)
                        if modifiers == KeyModifiers::CONTROL | KeyModifiers::ALT =>
                    {
                        self.give_all_back(cx)
                    }
                    (KeyCode::Char('r'), KeyModifiers::CONTROL) => self.give_back(cx),
                    (KeyCode::Up, _) => self.walk_cursor(-1),
                    (KeyCode::Down, _) => self.walk_cursor(1),
                    (KeyCode::PageUp, _) => self.walk_cursor(-(self.page as isize)),
                    (KeyCode::PageDown, _) => self.walk_cursor(self.page as isize),
                    (KeyCode::Home, _) => self.walk_cursor(isize::MIN / 2),
                    (KeyCode::End, _) => self.walk_cursor(isize::MAX / 2),
                    (KeyCode::Backspace, _) => {
                        self.query.pop();
                        self.scroll = 0;
                        self.cursor = 0;
                    }
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                        self.query.clear();
                        self.scroll = 0;
                        self.cursor = 0;
                    }
                    (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                        self.query.push(c);
                        self.scroll = 0;
                        self.cursor = 0;
                    }
                    _ => {}
                }
            }
            Event::Mouse(event) => match event.kind {
                MouseEventKind::ScrollDown => self.scroll(3),
                MouseEventKind::ScrollUp => self.scroll(-3),
                MouseEventKind::Down(helix_view::input::MouseButton::Left) => {
                    if let Some(index) = self.tabs.iter().position(|tab| {
                        event.row == tab.y && event.column >= tab.x && event.column < tab.right()
                    }) {
                        self.scope = index;
                        self.scroll = 0;
                        self.cursor = 0;
                        return EventResult::Consumed(None);
                    }
                    if let Some(index) = self.drawn.iter().position(|row| {
                        event.row == row.y && event.column >= row.x && event.column < row.right()
                    }) {
                        self.cursor = self.scroll + index;
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
    /// ones pressed, Backspace undoes the last of them, and everything else is a key.
    fn capturing(&mut self, key: KeyEvent, cx: &mut Context) {
        let empty = self
            .capture
            .as_ref()
            .is_some_and(|capture| capture.keys.is_empty());
        match key.code {
            KeyCode::Esc => self.capture = None,
            // With nothing pressed yet, Enter and Backspace are shortcuts like any other.
            KeyCode::Enter if !empty => {
                let refuses = self
                    .capture
                    .as_ref()
                    .and_then(|capture| capture.clash.as_ref())
                    .is_some_and(Clash::refuses);
                if !refuses {
                    self.give(cx);
                }
            }
            KeyCode::Backspace if !empty => {
                if let Some(capture) = self.capture.as_mut() {
                    capture.keys.pop();
                }
                self.weigh();
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
        let hint = match (&capture.clash, capture.keys.is_empty()) {
            (_, true) => "Esc goes back".to_string(),
            (Some(clash), _) if clash.refuses() => "Press others · Esc goes back".to_string(),
            (Some(Clash::Taken { .. }), _) => "Enter takes it · Esc goes back".to_string(),
            (Some(_), _) => "Enter gives it anyway · Esc goes back".to_string(),
            (None, _) => "Enter gives it · Backspace undoes a key · Esc goes back".to_string(),
        };

        let longest = [title.as_str(), pressed.as_str(), hint.as_str()]
            .into_iter()
            .chain(clash.as_deref())
            .map(|line| line.chars().count())
            .max()
            .unwrap_or(0);
        let width = (longest as u16 + 6).min(popup.width);
        let height = if clash.is_some() { 8 } else { 7 }.min(popup.height);
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

        // The tabs are the two worlds and what no key reaches: a panel's own arrows are
        // not shortcuts and are nowhere here.
        assert_eq!(SCOPES, ["All", "Insert", "Modal", "Unbound"]);
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
        let keys: Vec<KeyEvent> = vec!["C-A-j".parse().unwrap()];
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
        let key = |name: &str| name.parse::<KeyEvent>().unwrap();
        assert_eq!(
            screen.capture.as_ref().unwrap().was,
            vec![key("C-A-j")],
            "it knows what it is replacing"
        );

        // Its own keys are not in its own way.
        screen.press(key("C-A-j"));
        assert!(screen.capture.as_ref().unwrap().clash.is_none());

        // Another action's are.
        screen.capture.as_mut().unwrap().keys.clear();
        screen.press(key("C-A-k"));
        assert!(matches!(
            screen.capture.as_ref().unwrap().clash,
            Some(Clash::Taken { .. })
        ));
    }

    #[test]
    fn the_keys_pressed_are_the_ones_config_toml_would_write() {
        let key = |name: &str| name.parse::<KeyEvent>().unwrap();
        let shift_s = KeyEvent {
            code: KeyCode::Char('s'),
            modifiers: KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        };
        assert_eq!(settled(shift_s), key("C-S"));
        assert_eq!(settled(key("C-s")), key("C-s"));
    }
}
