//! What the editor shows when nothing is open: where to start, and where the help is.
use std::collections::HashMap;

use helix_view::{
    document::Mode,
    graphics::{Modifier, Rect},
    input::{KeyEvent, KeyModifiers},
    Editor,
};
use tui::buffer::Buffer as Surface;

use crate::{
    keymap::{KeyTrie, MappableCommand},
    ui::shortcuts::key_label,
};

/// Each line is what it does and the command it runs, as a keymap would name it.
const SECTIONS: &[(&str, &[(&str, &str)])] = &[
    (
        "Start",
        &[
            ("Open a file", "file_picker"),
            ("New file", ":new"),
            ("Search in the project", "global_search"),
            ("Show or hide the sidebar", "sidebar_toggle"),
        ],
    ),
    (
        "Help",
        &[
            ("Command palette", "command_palette"),
            ("Keyboard shortcuts", "keyboard_shortcuts"),
            ("Settings", "settings"),
            ("Check for updates", ":check-updates"),
        ],
    ),
];

const WIDTH: u16 = 52;

/// The name, as the wordmark draws it: the letters, and the cursor after them.
const LOGO: [(&str, &str); 6] = [
    ("                   ██", ""),
    ("        ▀▀         ██", ""),
    ("▄█▀▀▀▀  ██    ▄█▀▀▀██", ""),
    ("▀█▄▄▄   ██    ██   ██", ""),
    ("    ██  ██    ██   ██", ""),
    ("▀▀▀▀▀   ▀▀     ▀▀▀▀▀▀", "▀▀▀▀▀▀"),
];
const LOGO_WIDTH: u16 = 21;

#[derive(Default)]
pub struct Welcome {
    /// Where each line was drawn and what it runs, for a click to land on.
    items: Vec<(Rect, &'static str)>,
}

/// The keys bound to `command`, the one a hand reaches first: the Cmd ones on a Mac, the
/// others elsewhere, and a single key over a sequence.
fn keys_for(command: &str, bindings: &HashMap<String, Vec<Vec<KeyEvent>>>) -> Option<String> {
    let name = command.strip_prefix(':').unwrap_or(command);
    let on_mac = cfg!(target_os = "macos");
    bindings
        .get(name)?
        .iter()
        .min_by_key(|keys| {
            let cmd = keys
                .iter()
                .any(|key| key.modifiers.contains(KeyModifiers::SUPER));
            let labels: Vec<_> = keys.iter().map(|key| key_label(*key)).collect();
            (cmd != on_mac, keys.len(), labels.join(" ").len(), labels)
        })
        .map(|keys| {
            let labels: Vec<_> = keys.iter().map(|key| key_label(*key)).collect();
            labels.join(" → ")
        })
}

impl Welcome {
    pub fn render(
        &mut self,
        area: Rect,
        surface: &mut Surface,
        editor: &Editor,
        keymaps: &HashMap<Mode, KeyTrie>,
    ) {
        self.items.clear();
        let theme = &editor.theme;
        let text = theme.get("ui.text");
        let title = text.add_modifier(Modifier::BOLD);
        let dim = theme.get("ui.text.inactive");
        // A link's colour, without the underline a terminal breaks at every space.
        let link = match theme.try_get("markup.link.text").and_then(|style| style.fg) {
            Some(color) => text.fg(color),
            None => text,
        };
        // The name in the brightest text, and its cursor in the theme's blue: `info` is the
        // accent itself in the themes sid ships, the blue of the wordmark.
        let letters = theme.try_get("ui.text.focus").unwrap_or(text);
        let accent = match theme.get("info").fg {
            Some(color) => text.fg(color),
            None => link,
        };
        let bindings = keymaps
            .get(&editor.mode())
            .map(|keymap| crate::keymap::reachable(keymap.reverse_map(), editor.keyboard_enhanced))
            .unwrap_or_default();

        let width = WIDTH.min(area.width.saturating_sub(4));
        // A header, its lines and a blank per section; the hint.
        let menu = SECTIONS
            .iter()
            .map(|(_, items)| items.len() as u16 + 2)
            .sum::<u16>()
            + 1;
        // The name drawn large where there is room for it and the menu, in a line where not.
        let large = area.height >= LOGO.len() as u16 + 4 + menu && width >= LOGO_WIDTH + 8;
        let head = if large { LOGO.len() as u16 + 3 } else { 2 };
        let x = area.x + area.width.saturating_sub(width) / 2;
        let mut y = area.y + area.height.saturating_sub(head + menu) / 2;
        let bottom = area.bottom();
        let line = |y: &mut u16, draw: &mut dyn FnMut(u16)| {
            if *y < bottom {
                draw(*y);
            }
            *y += 1;
        };

        let version = crate::version::describe();
        if large {
            for (name, cursor) in LOGO {
                line(&mut y, &mut |y| {
                    surface.set_string(x, y, name, letters);
                    surface.set_string(x + LOGO_WIDTH + 2, y, cursor, accent);
                });
            }
            y += 1;
            line(&mut y, &mut |y| {
                surface.set_stringn(x, y, &version, width as usize, dim);
            });
        } else {
            line(&mut y, &mut |y| {
                surface.set_string(x, y, "sid", letters.add_modifier(Modifier::BOLD));
                surface.set_string(x + 3, y, "_", accent.add_modifier(Modifier::BOLD));
                surface.set_stringn(x + 6, y, &version, (width as usize).saturating_sub(6), dim);
            });
        }
        y += 1;

        for (header, items) in SECTIONS {
            line(&mut y, &mut |y| {
                surface.set_stringn(x, y, header, width as usize, title);
            });
            for (label, command) in items.iter() {
                let keys = keys_for(command, &bindings).unwrap_or_default();
                let items = &mut self.items;
                line(&mut y, &mut |y| {
                    let keys_width = keys.chars().count() as u16;
                    let label_width = width.saturating_sub(keys_width + 2).saturating_sub(2);
                    surface.set_stringn(x + 2, y, label, label_width as usize, link);
                    if keys_width + 2 <= width {
                        surface.set_stringn(
                            x + width - keys_width,
                            y,
                            &keys,
                            keys_width as usize,
                            dim,
                        );
                    }
                    items.push((Rect::new(x, y, width, 1), command));
                });
            }
            y += 1;
        }

        line(&mut y, &mut |y| {
            let hint = "Click a line, or press its keys";
            surface.set_stringn(x, y, hint, width as usize, dim);
        });
    }

    /// The command of the line drawn at the given cell, if any.
    pub fn command_at(&self, row: u16, column: u16) -> Option<MappableCommand> {
        self.items
            .iter()
            .find(|(area, _)| row == area.y && column >= area.x && column < area.x + area.width)
            .and_then(|(_, command)| command.parse().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_line_runs_a_command_that_exists() {
        for (_, items) in SECTIONS {
            for (label, command) in items.iter() {
                assert!(
                    command.parse::<MappableCommand>().is_ok(),
                    "{label}: no command named {command}"
                );
            }
        }
    }

    #[test]
    fn the_keys_shown_are_the_ones_a_hand_reaches_first() {
        let key = |name: &str| name.parse::<KeyEvent>().unwrap();
        let bindings = HashMap::from([(
            "file_picker".to_string(),
            vec![
                vec![key("space"), key("f")],
                vec![key("Cmd-p")],
                vec![key("C-p")],
            ],
        )]);
        let expected = if cfg!(target_os = "macos") {
            "Cmd+p"
        } else {
            "Ctrl+p"
        };
        assert_eq!(
            keys_for("file_picker", &bindings).as_deref(),
            Some(expected)
        );
        assert_eq!(keys_for(":new", &bindings), None);
    }
}
