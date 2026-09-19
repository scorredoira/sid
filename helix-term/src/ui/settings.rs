use std::path::{Path, PathBuf};

use anyhow::Context as _;
use helix_view::{
    document::Mode,
    editor::ConfigEvent,
    graphics::{Margin, Rect},
    input::{KeyCode, KeyEvent, MouseButton, MouseEventKind},
    theme::Modifier,
    Editor,
};
use serde_json::Value;
use tui::{
    buffer::Buffer as Surface,
    widgets::{Block, Widget},
};

use crate::compositor::{Component, Context, Event, EventResult};

/// What a setting can be: a switch, or one of a few words.
enum Kind {
    Switch,
    /// The words it cycles through, in order: what `config.toml` keeps, and what the
    /// screen reads it as when the two are not the same word.
    Words(&'static [(&'static str, &'static str)]),
}

/// One line of the screen: what it reads as, the configuration it stands for, and what
/// kind of answer it takes.
struct Setting {
    label: &'static str,
    /// The key as `config.toml` writes it, dots and all.
    key: &'static str,
    kind: Kind,
    /// What somebody would type looking for it, when that is not what it is called here:
    /// searched in the command palette, never drawn. Empty where the label is enough.
    also: &'static str,
}

/// The settings the screen offers. Everything else stays in `config.toml`, where the
/// whole of Helix's configuration lives.
const SETTINGS: &[Setting] = &[
    Setting {
        label: "The mode it opens in",
        key: "default-mode",
        // Modal editing is what somebody looks for; "normal" is Helix's word for it.
        // What config.toml calls "normal" is modal editing, and that is what it is
        // called here: in sid the normal thing is to type.
        kind: Kind::Words(&[("insert", "insert"), ("normal", "modal")]),
        also: "",
    },
    Setting {
        label: "Reopen the files a project had open",
        key: "restore-session",
        kind: Kind::Switch,
        also: "",
    },
    Setting {
        label: "Wrap long lines",
        key: "soft-wrap.enable",
        kind: Kind::Switch,
        also: "word wrap wordwrap soft wrap long lines",
    },
    Setting {
        label: "Save when you leave a file",
        key: "auto-save.focus-lost",
        kind: Kind::Switch,
        also: "",
    },
    Setting {
        label: "Save while you type",
        key: "auto-save.after-delay.enable",
        kind: Kind::Switch,
        also: "",
    },
    Setting {
        label: "Line numbers",
        key: "line-number",
        kind: Kind::Words(&[("absolute", "absolute"), ("relative", "relative")]),
        also: "",
    },
    Setting {
        label: "Tabs for the open files",
        key: "bufferline",
        kind: Kind::Words(&[
            ("multiple", "multiple"),
            ("always", "always"),
            ("never", "never"),
        ]),
        also: "",
    },
    Setting {
        label: "Indentation guides",
        key: "indent-guides.render",
        kind: Kind::Switch,
        also: "",
    },
    Setting {
        label: "The cursor while typing",
        key: "cursor-shape.insert",
        kind: Kind::Words(&[
            ("bar", "bar"),
            ("block", "block"),
            ("underline", "underline"),
        ]),
        also: "",
    },
    Setting {
        label: "The cursor blinks",
        key: "cursor-blink",
        kind: Kind::Switch,
        also: "",
    },
    Setting {
        label: "Highlight the line the cursor is on",
        key: "cursorline",
        kind: Kind::Switch,
        also: "",
    },
    Setting {
        label: "The mode colours the status line",
        key: "color-modes",
        kind: Kind::Switch,
        also: "",
    },
    Setting {
        label: "The picker obeys .gitignore",
        key: "file-picker.git-ignore",
        kind: Kind::Switch,
        also: "",
    },
    Setting {
        label: "The file tree hides files that start with a dot",
        key: "file-explorer.hidden",
        kind: Kind::Switch,
        also: "",
    },
    Setting {
        label: "The code, with the changes or the commits on screen",
        key: "sidebar.code",
        kind: Kind::Words(&[("beside", "beside them"), ("below", "under them")]),
        also: "sidebar layout position top bottom right below beside horizontal vertical",
    },
    Setting {
        label: "A commit's files",
        key: "sidebar.commit-files",
        kind: Kind::Words(&[("tree", "as a tree"), ("paths", "as paths")]),
        also: "commits files tree flat full path paths",
    },
    Setting {
        label: "The mouse",
        key: "mouse",
        kind: Kind::Switch,
        also: "",
    },
];

/// The settings a newcomer reaches for, each with what it is set to now and the keys
/// that flip it. Up and down walk them, typing narrows them, Space or Enter changes the
/// one in focus, a click changes the one it lands on, and every change is written to
/// `config.toml` as it is made.
pub struct Settings {
    /// Which of the shown settings is in focus.
    cursor: usize,
    /// What has been typed to narrow the list.
    query: String,
    /// The keys that flip each setting, by the command that does it.
    shortcuts: super::bindings::ByAction,
    /// Where each shown line was drawn last, so a click can land on one.
    rows: Vec<Rect>,
}

/// The room around the text inside the border.
const PADDING: u16 = 2;
/// The columns between the longest label and its value.
const GAP: u16 = 4;

impl Default for Settings {
    fn default() -> Self {
        Self::new(super::bindings::ByAction::new())
    }
}

impl Settings {
    /// The screen, told which keys run what so each setting can wear its own.
    pub fn new(shortcuts: super::bindings::ByAction) -> Self {
        Self {
            cursor: 0,
            query: String::new(),
            shortcuts,
            rows: Vec::new(),
        }
    }

    /// The settings the filter leaves, as indices into all of them.
    fn shown(&self) -> Vec<usize> {
        let words: Vec<String> = self
            .query
            .to_lowercase()
            .split_whitespace()
            .map(ToString::to_string)
            .collect();
        (0..SETTINGS.len())
            .filter(|index| {
                let setting = &SETTINGS[*index];
                let text =
                    format!("{} {} {}", setting.label, setting.also, setting.key).to_lowercase();
                words.iter().all(|word| text.contains(word))
            })
            .collect()
    }

    /// The keys that flip a setting, as the keyboard shortcuts screen writes them.
    fn keys_of(&self, setting: &Setting) -> String {
        let runs = super::bindings::Runs::One(format!(":toggle-option {}", given(setting)));
        self.shortcuts
            .get(&runs)
            .map(|shortcuts| {
                shortcuts
                    .iter()
                    .map(|keys| {
                        keys.iter()
                            .map(|key| super::shortcuts::key_label(*key))
                            .collect::<Vec<_>>()
                            .join(" → ")
                    })
                    .collect::<Vec<_>>()
                    .join("   ")
            })
            .unwrap_or_default()
    }

    fn walk(&mut self, down: bool) {
        let count = self.shown().len();
        if count == 0 {
            self.cursor = 0;
            return;
        }
        self.cursor = if down {
            (self.cursor + 1) % count
        } else {
            self.cursor.checked_sub(1).unwrap_or(count - 1)
        };
    }

    /// Changes the setting in focus: a switch flips, a word gives way to the next one.
    fn change(&mut self, cx: &mut Context) {
        let Some(&index) = self.shown().get(self.cursor) else {
            return;
        };
        let setting = &SETTINGS[index];
        let current = read(&snapshot(cx.editor), setting.key);
        let next = match (&setting.kind, &current) {
            (Kind::Switch, Value::Bool(on)) => Value::Bool(!on),
            (Kind::Words(words), Value::String(word)) => {
                let at = words.iter().position(|(value, _)| value == word);
                let next = at.map_or(0, |at| (at + 1) % words.len());
                Value::String(words[next].0.to_string())
            }
            _ => {
                cx.editor
                    .set_error(format!("'{}' is not a setting we can change", setting.key));
                return;
            }
        };

        if let Err(err) = flip(cx.editor, setting.key, &next) {
            log::error!("Could not change '{}': {err:#}", setting.key);
            cx.editor.set_error(format!("{err:#}"));
        }
    }
}

/// Puts a setting's new value in the running editor and writes it to `config.toml`: a
/// setting is a command, and wherever it is flipped — this screen, the palette, a key —
/// it is written. Only the settings this screen offers are: what else `:toggle-option`
/// can flip is Helix's, and stays for the session as it always has.
pub(crate) fn flip(editor: &mut Editor, key: &str, value: &Value) -> anyhow::Result<()> {
    apply(editor, key, value).context("Could not change it")?;

    // The mode it opens in is the one to be in now as well: changing it and waiting
    // for the next file to be opened would read as the setting not working.
    if key == "default-mode" {
        match value {
            Value::String(mode) if mode == "insert" => editor.mode = Mode::Insert,
            _ => editor.enter_normal_mode(),
        }
    }

    write_setting(&helix_loader::config_file(), key, value).context("Changed, but not written down")
}

/// Whether `:toggle-option <key>` flips one of the settings of this screen.
pub(crate) fn is_setting(key: &str) -> bool {
    named(key).is_some()
}

/// What a setting reads as when it is named by key: what `:toggle <key>` does, in the
/// words the settings screen uses, so a shortcut for it says something.
pub(crate) fn told(args: &str) -> Option<String> {
    Some(said(named(args)?))
}

/// The setting `:toggle-option` was given, which is the first word of its arguments: the
/// rest, where there is any, are the values it walks.
fn named(args: &str) -> Option<&'static Setting> {
    let key = args.split_whitespace().next()?;
    SETTINGS.iter().find(|setting| setting.key == key)
}

fn said(setting: &Setting) -> String {
    match setting.kind {
        Kind::Switch => format!("{}, on or off", setting.label),
        Kind::Words(_) => format!("{}, the next one", setting.label),
    }
}

/// The other words somebody might look for a setting by, for the palette to search and
/// never show.
pub(crate) fn also(args: &str) -> &'static str {
    named(args).map(|setting| setting.also).unwrap_or_default()
}

/// Every setting as the command that flips it, for the command palette: the screen's
/// settings are things the editor does, so they are looked for and given keys like
/// anything else it does. With what `:toggle-option` is given, what it reads as, and
/// the other words it answers to.
pub(crate) fn as_commands() -> Vec<(String, String, String)> {
    SETTINGS
        .iter()
        .map(|setting| (given(setting), said(setting), setting.also.to_string()))
        .collect()
}

/// What `:toggle-option` has to be given for the setting: its key, and for one that is a
/// few words, the words themselves, because that is how the command walks them. Without
/// them it answers "Bad arguments" and the shortcut does nothing.
fn given(setting: &Setting) -> String {
    match setting.kind {
        Kind::Switch => setting.key.to_string(),
        Kind::Words(words) => {
            let values: Vec<_> = words.iter().map(|(value, _)| *value).collect();
            format!("{} {}", setting.key, values.join(" "))
        }
    }
}

/// The whole configuration as it is right now, read once: every setting is looked up in
/// it, and turning the editor's configuration into JSON once per setting per frame is
/// most of what drawing the screen would cost.
fn snapshot(editor: &Editor) -> Value {
    serde_json::json!(&*editor.config())
}

/// What a setting is set to in a snapshot of the configuration.
fn read(config: &Value, key: &str) -> Value {
    let pointer = format!("/{}", key.replace('.', "/"));

    config.pointer(&pointer).cloned().unwrap_or(Value::Null)
}

/// Puts the new value in the running editor, which redraws with it at once.
pub(crate) fn apply(editor: &mut Editor, key: &str, value: &Value) -> anyhow::Result<()> {
    let mut config = serde_json::json!(&*editor.config());
    let pointer = format!("/{}", key.replace('.', "/"));
    let at = config
        .pointer_mut(&pointer)
        .with_context(|| format!("'{key}' is not a setting"))?;
    *at = value.clone();

    let config = serde_json::from_value(config).context("the change does not fit the settings")?;
    editor
        .config_events
        .0
        .send(ConfigEvent::Update(config))
        .context("the editor did not take the change")?;

    Ok(())
}

/// Writes one setting into the user's `config.toml`, leaving every other line of it —
/// its comments and its order included — exactly as it was.
pub(crate) fn write_setting(path: &Path, key: &str, value: &Value) -> anyhow::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };

    let mut document: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("{} is not valid TOML", path.display()))?;

    // Every setting of ours lives under [editor], and the key's dots are tables.
    let mut table = document
        .as_table_mut()
        .entry("editor")
        .or_insert_with(implicit_table);
    let mut names = key.split('.').peekable();
    while let Some(name) = names.next() {
        if names.peek().is_none() {
            table
                .as_table_like_mut()
                .with_context(|| format!("'{name}' is not a table in {}", path.display()))?
                .insert(name, toml_edit::value(as_toml(value)?));
            break;
        }

        table = table
            .as_table_like_mut()
            .with_context(|| format!("'{name}' is not a table in {}", path.display()))?
            .entry(name)
            .or_insert(implicit_table());
    }

    write_atomically(path, document.to_string())
}

/// A table nobody asked for by name: it is written only if it ends up holding something,
/// so a fresh file gets `[editor.auto-save]` and not an empty `[editor]` above it.
fn implicit_table() -> toml_edit::Item {
    let mut table = toml_edit::Table::new();
    table.set_implicit(true);

    toml_edit::Item::Table(table)
}

fn as_toml(value: &Value) -> anyhow::Result<toml_edit::Value> {
    match value {
        Value::Bool(on) => Ok((*on).into()),
        Value::String(word) => Ok(word.as_str().into()),
        Value::Number(number) if number.is_i64() => Ok(number
            .as_i64()
            .expect("an integer was just checked for")
            .into()),
        Value::Number(number) => Ok(number
            .as_f64()
            .expect("a number is an integer or a float")
            .into()),
        other => anyhow::bail!("{other} cannot be written to config.toml"),
    }
}

/// Written aside and renamed over, so a crash never leaves half a configuration. A
/// config.toml that is a link to a file elsewhere — a dotfiles checkout, say — is
/// written where it points: renaming over the link would turn it into a file of its
/// own, and the checkout would never see the change.
pub(crate) fn write_atomically(path: &Path, text: String) -> anyhow::Result<()> {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let path = path.as_path();
    let directory = path
        .parent()
        .context("the configuration file sits in a directory")?;
    std::fs::create_dir_all(directory)
        .with_context(|| format!("creating {}", directory.display()))?;

    let temporary: PathBuf = directory.join(format!(".config.{}.toml", std::process::id()));
    std::fs::write(&temporary, text).with_context(|| format!("writing {}", temporary.display()))?;
    std::fs::rename(&temporary, path).with_context(|| format!("replacing {}", path.display()))?;

    Ok(())
}

/// How a setting's value reads on screen, which is not always the word config.toml keeps.
fn shown(setting: &Setting, value: &Value) -> String {
    match value {
        Value::Bool(true) => "on".to_string(),
        Value::Bool(false) => "off".to_string(),
        Value::String(word) => match &setting.kind {
            Kind::Words(words) => words
                .iter()
                .find(|(value, _)| value == word)
                .map(|(_, label)| label.to_string())
                .unwrap_or_else(|| word.clone()),
            Kind::Switch => word.clone(),
        },
        other => other.to_string(),
    }
}

impl Component for Settings {
    fn render(&mut self, area: Rect, surface: &mut Surface, cx: &mut Context) {
        let config = snapshot(cx.editor);
        let labels = SETTINGS
            .iter()
            .map(|setting| setting.label.chars().count())
            .max()
            .unwrap_or(0) as u16;
        let keys: Vec<String> = SETTINGS
            .iter()
            .map(|setting| self.keys_of(setting))
            .collect();
        let keys_width = keys
            .iter()
            .map(|keys| keys.chars().count())
            .max()
            .unwrap_or(0) as u16;
        let values = SETTINGS
            .iter()
            .map(|setting| shown(setting, &read(&config, setting.key)).chars().count())
            .max()
            .unwrap_or(0) as u16;

        let hint = "Space changes it  ·  Type to filter  ·  Esc closes";
        let inside = (labels + GAP + keys_width + GAP + values).max(hint.chars().count() as u16);
        let width = (inside + PADDING * 2 + 2).min(area.width);
        // The title, the filter, a blank row, the settings, a blank row and the hint, plus
        // borders.
        let height = (SETTINGS.len() as u16 + 7).min(area.height);
        let screen = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        );

        let theme = &cx.editor.theme;
        let background = theme.get("ui.popup");
        let text = theme.get("ui.text");
        let selected = theme.get("ui.menu").patch(theme.get("ui.menu.selected"));
        let value_style = theme.get("constant");
        let dim = theme.get("ui.text.inactive");

        surface.clear_with(screen, background);
        let block = Block::bordered().style(background);
        let inner = block.inner(screen).inner(Margin::horizontal(PADDING - 1));
        block.render(screen, surface);

        let bold = text.add_modifier(Modifier::BOLD);
        surface.set_stringn(inner.x, inner.y, "Settings", inner.width as usize, bold);
        let filter = format!("Filter: {}▏", self.query);
        surface.set_stringn(inner.x, inner.y + 1, &filter, inner.width as usize, text);

        let shown_settings = self.shown();
        self.cursor = self.cursor.min(shown_settings.len().saturating_sub(1));
        self.rows.clear();
        for (at, index) in shown_settings.iter().enumerate() {
            let setting = &SETTINGS[*index];
            let y = inner.y + 3 + at as u16;
            if y >= inner.bottom() {
                break;
            }

            let focused = at == self.cursor;
            let row = Rect::new(inner.x, y, inner.width, 1);
            if focused {
                surface.set_style(row, selected);
            }

            let label_style = if focused { selected } else { text };
            surface.set_stringn(inner.x, y, setting.label, inner.width as usize, label_style);

            let value = shown(setting, &read(&config, setting.key));
            let value_x = inner.right().saturating_sub(value.chars().count() as u16);
            let style = if focused { selected } else { value_style };
            surface.set_stringn(value_x, y, &value, inner.width as usize, style);

            // The keys that flip it, between the label and the value: a setting is a
            // command, and this is where its shortcut is seen without leaving the screen.
            let keys = &keys[*index];
            if !keys.is_empty() {
                let keys_x = inner.x + labels + GAP;
                let room = value_x.saturating_sub(keys_x + 1) as usize;
                surface.set_stringn(keys_x, y, keys, room, if focused { selected } else { dim });
            }

            self.rows.push(row);
        }

        let y = inner.bottom().saturating_sub(1);
        surface.set_stringn(inner.x, y, hint, inner.width as usize, dim);
    }

    fn handle_event(&mut self, event: &Event, cx: &mut Context) -> EventResult {
        match event {
            Event::Key(key) => self.handle_key(*key, cx),
            Event::Mouse(event) => {
                if event.kind != MouseEventKind::Down(MouseButton::Left) {
                    return EventResult::Consumed(None);
                }

                let hit = self.rows.iter().position(|row| {
                    event.row == row.y && event.column >= row.x && event.column < row.right()
                });
                if let Some(index) = hit {
                    self.cursor = index;
                    self.change(cx);
                }

                EventResult::Consumed(None)
            }
            // The screen is modal: nothing behind it hears a thing until it closes.
            _ => EventResult::Consumed(None),
        }
    }
}

impl Settings {
    fn handle_key(&mut self, key: KeyEvent, cx: &mut Context) -> EventResult {
        match key.code {
            KeyCode::Esc => {
                return EventResult::Consumed(Some(Box::new(|compositor, _| {
                    compositor.pop();
                })))
            }
            KeyCode::Down | KeyCode::Tab => self.walk(true),
            KeyCode::Up => self.walk(false),
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right => self.change(cx),
            KeyCode::Backspace => {
                self.query.pop();
                self.cursor = 0;
            }
            // Typing narrows the list: the words of a setting, or the key config.toml
            // knows it by.
            KeyCode::Char(c) if !key.modifiers.intersects(HELD) => {
                self.query.push(c);
                self.cursor = 0;
            }
            _ => {}
        }

        EventResult::Consumed(None)
    }
}

/// The modifiers that make a letter a shortcut rather than something typed.
const HELD: helix_view::keyboard::KeyModifiers = helix_view::keyboard::KeyModifiers::CONTROL
    .union(helix_view::keyboard::KeyModifiers::ALT)
    .union(helix_view::keyboard::KeyModifiers::SUPER);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_setting_of_a_few_words_is_given_them_or_it_cannot_be_flipped() {
        for (args, doc, _) in as_commands() {
            let command: crate::commands::MappableCommand =
                format!(":toggle-option {args}").parse().unwrap();
            let crate::commands::MappableCommand::Typable { args, .. } = &command else {
                panic!("a setting flips through the command line");
            };
            let setting = named(args).expect("it names a setting of this screen");
            match setting.kind {
                // `:toggle-option <key>` on a switch, and nothing else.
                Kind::Switch => assert_eq!(args.split_whitespace().count(), 1, "{doc}"),
                // And every word of one that is a few, in the order it walks them.
                Kind::Words(words) => {
                    let given: Vec<_> = args.split_whitespace().skip(1).collect();
                    let expected: Vec<_> = words.iter().map(|(value, _)| *value).collect();
                    assert_eq!(given, expected, "{doc}");
                }
            }
        }
    }

    #[test]
    fn the_word_on_screen_is_not_always_the_word_config_toml_keeps() {
        let setting = SETTINGS
            .iter()
            .find(|setting| setting.key == "default-mode")
            .expect("the mode it opens in is a setting");
        // What Helix calls normal mode is modal editing, and that is what it reads as.
        assert_eq!(
            shown(setting, &Value::String("normal".into())),
            "modal".to_string()
        );
        assert_eq!(
            shown(setting, &Value::String("insert".into())),
            "insert".to_string()
        );
    }

    #[test]
    fn a_setting_is_written_where_it_belongs() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("config.toml");
        std::fs::write(
            &path,
            "# mine, and it stays\ntheme = \"github_dark\"\n\n[editor]\nmouse = true\n",
        )
        .unwrap();

        write_setting(&path, "soft-wrap.enable", &Value::Bool(false)).unwrap();
        write_setting(&path, "line-number", &Value::String("relative".to_string())).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();

        assert!(written.contains("# mine, and it stays"));
        assert!(written.contains("theme = \"github_dark\""));
        assert!(written.contains("mouse = true"));
        assert!(written.contains("line-number = \"relative\""));
        assert!(written.contains("[editor.soft-wrap]\nenable = false"));
    }

    #[test]
    fn the_cursor_while_typing_is_a_setting_that_reads_back() {
        let mut config = serde_json::json!(helix_view::editor::Config::default());
        let at = config.pointer_mut("/cursor-shape/insert").unwrap();
        *at = Value::String("block".to_string());
        let config: helix_view::editor::Config = serde_json::from_value(config).unwrap();
        assert_eq!(
            config
                .cursor_shape
                .from_mode(helix_view::document::Mode::Insert),
            helix_view::graphics::CursorKind::Block
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_config_that_is_a_link_is_written_where_it_points() {
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("dotfiles").join("sid.toml");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, "theme = \"github_dark\"\n").unwrap();
        let link = directory.path().join("config.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        write_setting(&link, "mouse", &Value::Bool(false)).unwrap();

        assert!(std::fs::symlink_metadata(&link).unwrap().is_symlink());
        let written = std::fs::read_to_string(&real).unwrap();
        assert!(written.contains("theme = \"github_dark\""));
        assert!(written.contains("mouse = false"), "{written}");
    }

    #[test]
    fn typing_narrows_the_settings_by_every_word_for_them() {
        let mut settings = Settings::default();
        assert_eq!(settings.shown().len(), SETTINGS.len());
        settings.query = "wrap".into();
        let shown = settings.shown();
        assert_eq!(shown.len(), 1);
        assert_eq!(SETTINGS[shown[0]].key, "soft-wrap.enable");
        // By the other words it answers to, and by its key in config.toml.
        settings.query = "wordwrap".into();
        assert_eq!(settings.shown().len(), 1);
        settings.query = "cursor-blink".into();
        assert_eq!(settings.shown().len(), 1);
    }

    #[test]
    fn a_setting_wears_the_keys_that_flip_it() {
        let trie: crate::keymap::KeyTrie =
            toml::from_str(r#"A-z = ":toggle soft-wrap.enable""#).unwrap();
        let settings = Settings::new(super::super::bindings::by_action(&trie, true));
        let wrapping = SETTINGS
            .iter()
            .find(|setting| setting.key == "soft-wrap.enable")
            .unwrap();
        assert_eq!(settings.keys_of(wrapping), "Alt+z");
        let mouse = SETTINGS
            .iter()
            .find(|setting| setting.key == "mouse")
            .unwrap();
        assert_eq!(settings.keys_of(mouse), "");
    }

    #[test]
    fn a_file_that_is_not_there_yet_is_written_whole() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sid").join("config.toml");

        write_setting(&path, "auto-save.focus-lost", &Value::Bool(true)).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, "[editor.auto-save]\nfocus-lost = true\n");
    }
}
