//! A read-only reference of the actual configured bindings, not a command launcher.
use std::collections::HashMap;

use helix_view::{
    document::Mode,
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
};

struct Shortcut {
    scope: &'static str,
    keys: String,
    description: String,
    search: String,
}

impl Shortcut {
    fn new(scope: &'static str, keys: String, description: String, command: &str) -> Self {
        let description = description.split_whitespace().collect::<Vec<_>>().join(" ");
        let search = format!("{scope} {keys} {description} {command}").to_lowercase();
        Self {
            scope,
            keys,
            description,
            search,
        }
    }
}

const SCOPES: &[&str] = &["All", "Insert", "Normal", "Select", "Sidebar"];

pub struct Shortcuts {
    entries: Vec<Shortcut>,
    query: String,
    scope: usize,
    scroll: usize,
    page: usize,
    tabs: Vec<Rect>,
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

fn collect(
    node: &KeyTrie,
    scope: &'static str,
    path: &mut Vec<String>,
    entries: &mut Vec<Shortcut>,
    enhanced: bool,
) {
    match node {
        KeyTrie::Node(node) => {
            for (key, child) in node.iter() {
                // A key this terminal never sends is not a shortcut here.
                if !crate::keymap::key_reaches(key, enhanced) {
                    continue;
                }
                path.push(key_label(*key));
                collect(child, scope, path, entries, enhanced);
                path.pop();
            }
        }
        KeyTrie::MappableCommand(command) if command.name() != "no_op" => {
            entries.push(Shortcut::new(
                scope,
                path.join(" → "),
                command.doc().into(),
                command.name(),
            ));
        }
        KeyTrie::Sequence(commands) => {
            let descriptions: Vec<_> = commands.iter().map(|command| command.doc()).collect();
            let names: Vec<_> = commands.iter().map(|command| command.name()).collect();
            entries.push(Shortcut::new(
                scope,
                path.join(" → "),
                descriptions.join("; "),
                &names.join(" "),
            ));
        }
        _ => {}
    }
}

impl Shortcuts {
    pub fn new(maps: &HashMap<Mode, KeyTrie>, enhanced: bool) -> Self {
        let mut entries = Vec::new();
        for (mode, scope) in [
            (Mode::Insert, "Insert"),
            (Mode::Normal, "Normal"),
            (Mode::Select, "Select"),
        ] {
            if let Some(map) = maps.get(&mode) {
                collect(map, scope, &mut Vec::new(), &mut entries, enhanced);
            }
        }
        // These are local controls, handled before the configurable editor keymaps.
        for (scope, keys, description) in [
            ("Sidebar", "↑ / ↓", "Move through the focused list"),
            (
                "Sidebar",
                "PageUp / PageDown",
                "Move one page through the focused list",
            ),
            (
                "Sidebar",
                "Home / End",
                "First / last item in the focused list",
            ),
            (
                "Sidebar",
                "Enter / →",
                "Open the selected item or expand its directory",
            ),
            ("Sidebar", "←", "Collapse a directory or go to its parent"),
            ("Sidebar", "Shift+←", "Collapse all directories"),
            ("Sidebar", "Tab", "Switch Files / Changes / Commits"),
            ("Sidebar", "Esc", "Go back, or return focus to code"),
            ("Sidebar", "F5", "Refresh the current list"),
            (
                "Files",
                "/",
                "Filter the file tree by name; Esc clears the filter",
            ),
            (
                "Files",
                "Ctrl+Alt+n",
                "Create a file or directory, from anywhere",
            ),
            (
                "Files",
                "Ctrl+Alt+r",
                "Rename the selected file or directory, from anywhere",
            ),
            (
                "Files",
                "Shift+Delete",
                "Delete the selected item after confirmation, from anywhere",
            ),
            ("Files", ".", "Show or hide hidden files"),
            (
                "Files",
                "Delete",
                "Delete the selected item after confirmation",
            ),
            (
                "Files",
                "Click / double click",
                "Show the file and stay in the tree / open it and go to the code",
            ),
            (
                "Files",
                "Ctrl+Alt+o",
                "Show or hide the outline of the file's functions and types under the tree, from anywhere",
            ),
            (
                "Files",
                "Alt+↑ / Alt+↓",
                "Focus the file tree / the outline",
            ),
            (
                "Outline",
                "Click / Enter",
                "Go to the definition and stay in the outline / go to the code",
            ),
            (
                "Outline",
                "Click on \"by name\" or \"by position\"",
                "List the definitions by name, or in the file's order",
            ),
            (
                "Outline",
                "Right click",
                "List only functions and methods or every definition, and put the outline under the tree or beside it",
            ),
            (
                "Changes",
                "Click / Enter",
                "Show what the selected file changed",
            ),
            ("Changes", "o", "Open the selected file to edit it"),
            ("Changes", "s / u", "Stage / unstage the selected file"),
            (
                "Changes",
                "d / Delete",
                "Discard the selected file's changes after confirmation",
            ),
            (
                "Commits",
                "/",
                "Filter commits by hash, subject or author; Esc clears the filter",
            ),
            (
                "Commits",
                "Alt+↑ / Alt+↓",
                "Focus commit history / commit files",
            ),
            (
                "Commits",
                "Double click",
                "Show or hide the files the commit touched",
            ),
        ] {
            entries.push(Shortcut::new(scope, keys.into(), description.into(), ""));
        }
        entries.sort_by(|a, b| (a.scope, &a.keys).cmp(&(b.scope, &b.keys)));
        Self {
            entries,
            query: String::new(),
            scope: 0,
            scroll: 0,
            page: 1,
            tabs: Vec::new(),
        }
    }

    fn matches(&self, entry: &Shortcut) -> bool {
        let scope = SCOPES[self.scope];
        let in_scope = scope == "All"
            || scope == entry.scope
            || (scope == "Sidebar"
                && matches!(entry.scope, "Files" | "Outline" | "Changes" | "Commits"));
        in_scope
            && self
                .query
                .to_lowercase()
                .split_whitespace()
                .all(|word| entry.search.contains(word))
    }

    fn scroll(&mut self, delta: isize) {
        let count = self
            .entries
            .iter()
            .filter(|entry| self.matches(entry))
            .count();
        self.scroll = (self.scroll as isize + delta)
            .clamp(0, count.saturating_sub(self.page) as isize) as usize;
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
        let area = block.inner(popup);
        block.render(popup, surface);
        self.tabs.clear();
        if area.width < 12 || area.height < 8 {
            return;
        }
        let x = area.x + 2;
        let width = area.width.saturating_sub(4) as usize;
        let title = theme.get("ui.text").add_modifier(Modifier::BOLD);
        let dim = theme.get("ui.text.inactive");
        surface.set_stringn(x, area.y, "Keyboard shortcuts", width, title);
        // The build, so a report can say exactly which sid it came from.
        let version = format!("sid {}", helix_loader::VERSION_AND_GIT_HASH);
        let version_width = version.chars().count();
        if version_width + "Keyboard shortcuts".len() + 2 <= width {
            let version_x = x + (width - version_width) as u16;
            surface.set_stringn(version_x, area.y, &version, version_width, dim);
        }
        self.tabs.clear();
        let mut at = x;
        for (index, scope) in SCOPES.iter().enumerate() {
            let style = if index == self.scope {
                theme.get("ui.menu.selected").patch(title)
            } else {
                dim
            };
            let room = area.right().saturating_sub(at + 1) as usize;
            let (end, _) = surface.set_stringn(at, area.y + 1, scope, room, style);
            self.tabs
                .push(Rect::new(at, area.y + 1, end.saturating_sub(at), 1));
            at = end.saturating_add(3);
        }
        surface.set_stringn(
            x,
            area.y + 2,
            "Normal mode temporarily: Ctrl+Shift+P → normal_mode · i: insert",
            width,
            dim,
        );
        let filter = format!("Filter: {}▏", self.query);
        surface.set_stringn(x, area.y + 3, &filter, width, theme.get("ui.text"));
        let entries: Vec<_> = self
            .entries
            .iter()
            .filter(|entry| self.matches(entry))
            .collect();
        self.page = area.height.saturating_sub(7) as usize;
        self.scroll = self.scroll.min(entries.len().saturating_sub(self.page));
        let key_width = (width / 3).clamp(12, 36).min(width);
        let scope_width = 9usize.min(width.saturating_sub(key_width));
        let description_width = width.saturating_sub(key_width + scope_width);
        surface.set_stringn(x, area.y + 5, "Shortcut", key_width, dim);
        surface.set_stringn(x + key_width as u16, area.y + 5, "Where", scope_width, dim);
        surface.set_stringn(
            x + (key_width + scope_width) as u16,
            area.y + 5,
            "Action",
            description_width,
            dim,
        );
        for (index, entry) in entries.iter().skip(self.scroll).take(self.page).enumerate() {
            let y = area.y + 6 + index as u16;
            surface.set_string_truncated(
                x,
                y,
                &entry.keys,
                key_width.saturating_sub(2),
                |_| title,
                true,
                false,
            );
            surface.set_stringn(
                x + key_width as u16,
                y,
                entry.scope,
                scope_width.saturating_sub(1),
                dim,
            );
            surface.set_string_truncated(
                x + (key_width + scope_width) as u16,
                y,
                &entry.description,
                description_width,
                |_| theme.get("ui.text"),
                true,
                false,
            );
        }
        let footer = format!(
            "{} shortcuts · Type to filter · Tab: scope · ↑↓ / wheel: scroll · Esc / Shift+F1: close",
            entries.len()
        );
        surface.set_stringn(x, area.bottom() - 1, &footer, width, dim);
    }

    fn handle_event(&mut self, event: &Event, _cx: &mut Context) -> EventResult {
        match event {
            Event::Key(key) => match (key.code, key.modifiers) {
                (KeyCode::Esc | KeyCode::F(1), _) => {
                    return EventResult::Consumed(Some(Box::new(|compositor, _| {
                        compositor.pop();
                    })))
                }
                (KeyCode::Tab, _) => {
                    self.scope = (self.scope + 1) % SCOPES.len();
                    self.scroll = 0;
                }
                (KeyCode::Up, _) => self.scroll(-1),
                (KeyCode::Down, _) => self.scroll(1),
                (KeyCode::PageUp, _) => self.scroll(-(self.page as isize)),
                (KeyCode::PageDown, _) => self.scroll(self.page as isize),
                (KeyCode::Home, _) => self.scroll = 0,
                (KeyCode::End, _) => self.scroll(isize::MAX / 2),
                (KeyCode::Backspace, _) => {
                    self.query.pop();
                    self.scroll = 0;
                }
                (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                    self.query.clear();
                    self.scroll = 0;
                }
                (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                    self.query.push(c);
                    self.scroll = 0;
                }
                _ => {}
            },
            Event::Mouse(event) => match event.kind {
                MouseEventKind::ScrollDown => self.scroll(3),
                MouseEventKind::ScrollUp => self.scroll(-3),
                MouseEventKind::Down(helix_view::input::MouseButton::Left) => {
                    if let Some(index) = self.tabs.iter().position(|tab| {
                        event.row == tab.y && event.column >= tab.x && event.column < tab.right()
                    }) {
                        self.scope = index;
                        self.scroll = 0;
                    }
                }
                _ => {}
            },
            _ => {}
        }
        EventResult::Consumed(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_includes_custom_sequences_macros_and_local_sidebar_keys() {
        let trie: KeyTrie = toml::from_str(
            r#"
            C-P = "keyboard_shortcuts"
            F3 = ["move_char_right", "move_char_left"]
            F4 = "@mihello<esc>"
            F5 = "no_op"
            [space]
            h = "file_history"
        "#,
        )
        .unwrap();
        let maps = HashMap::from([(Mode::Insert, trie)]);
        let mut screen = Shortcuts::new(&maps, true);
        assert!(screen
            .entries
            .iter()
            .any(|entry| entry.keys == "Ctrl+Shift+p" && entry.description.contains("shortcuts")));
        assert!(screen.entries.iter().any(|entry| entry.keys == "Space → h"));
        assert!(screen
            .entries
            .iter()
            .any(|entry| entry.keys == "F3" && entry.description.contains(';')));
        assert!(screen.entries.iter().any(|entry| entry.keys == "F4"));
        assert!(!screen
            .entries
            .iter()
            .any(|entry| entry.scope == "Insert" && entry.keys == "F5"));
        screen.query = "commit history".into();
        screen.scope = 4;
        let found: Vec<_> = screen
            .entries
            .iter()
            .filter(|entry| screen.matches(entry))
            .collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].scope, "Commits");
    }
}
