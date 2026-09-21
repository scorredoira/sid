use crate::keymap;
use crate::keymap::{merge_keys, KeyTrie};
use helix_loader::merge_toml_values;
use helix_view::{document::Mode, theme};
use serde::Deserialize;
use std::collections::HashMap;
use std::fmt::Display;
use std::fs;
use std::io::Error as IOError;
use toml::de::Error as TomlError;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub theme: Option<theme::Config>,
    pub keys: HashMap<Mode, KeyTrie>,
    pub editor: helix_view::editor::Config,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigRaw {
    pub theme: Option<theme::Config>,
    pub keys: Option<HashMap<Mode, KeyTrie>>,
    pub editor: Option<toml::Value>,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            theme: None,
            keys: keymap::default(),
            editor: helix_view::editor::Config::default(),
        }
    }
}

#[derive(Debug)]
pub enum ConfigLoadError {
    BadConfig(TomlError),
    Error(IOError),
}

impl Default for ConfigLoadError {
    fn default() -> Self {
        ConfigLoadError::Error(IOError::new(std::io::ErrorKind::NotFound, "place holder"))
    }
}

impl Display for ConfigLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigLoadError::BadConfig(err) => err.fmt(f),
            ConfigLoadError::Error(err) => err.fmt(f),
        }
    }
}

impl Config {
    pub fn load(
        global: Result<&String, ConfigLoadError>,
        local: Result<String, ConfigLoadError>,
    ) -> Result<Config, ConfigLoadError> {
        let global_config: Result<ConfigRaw, ConfigLoadError> =
            global.and_then(|file| toml::from_str(file).map_err(ConfigLoadError::BadConfig));
        let local_config: Result<ConfigRaw, ConfigLoadError> =
            local.and_then(|file| toml::from_str(&file).map_err(ConfigLoadError::BadConfig));
        let res = match (global_config, local_config) {
            (Ok(global), Ok(local)) => {
                let mut keys = keymap::default();
                if let Some(global_keys) = global.keys {
                    merge_keys(&mut keys, global_keys)
                }
                if let Some(local_keys) = local.keys {
                    merge_keys(&mut keys, local_keys)
                }

                let editor = match (global.editor, local.editor) {
                    (None, None) => helix_view::editor::Config::default(),
                    (None, Some(val)) | (Some(val), None) => {
                        val.try_into().map_err(ConfigLoadError::BadConfig)?
                    }
                    (Some(global), Some(local)) => merge_toml_values(global, local, 3)
                        .try_into()
                        .map_err(ConfigLoadError::BadConfig)?,
                };

                Config {
                    theme: local.theme.or(global.theme),
                    keys,
                    editor,
                }
            }
            // if any configs are invalid return that first
            (_, Err(ConfigLoadError::BadConfig(err)))
            | (Err(ConfigLoadError::BadConfig(err)), _) => {
                return Err(ConfigLoadError::BadConfig(err))
            }
            (Ok(config), Err(_)) | (Err(_), Ok(config)) => {
                let mut keys = keymap::default();
                if let Some(keymap) = config.keys {
                    merge_keys(&mut keys, keymap);
                }
                Config {
                    theme: config.theme,
                    keys,
                    editor: config.editor.map_or_else(
                        || Ok(helix_view::editor::Config::default()),
                        |val| val.try_into().map_err(ConfigLoadError::BadConfig),
                    )?,
                }
            }

            // these are just two io errors return the one for the global config
            (Err(err), Err(_)) => return Err(err),
        };

        Ok(res)
    }

    pub fn load_default() -> Result<Config, ConfigLoadError> {
        // No config.toml is not an error: sid's defaults are a configuration of their own.
        let user_config = match fs::read_to_string(helix_loader::config_file()) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(ConfigLoadError::Error(err)),
        };
        let global_config = over_defaults(&user_config)?;
        let local_config = fs::read_to_string(helix_loader::workspace_config_file())
            .map_err(ConfigLoadError::Error)
            .and_then(|text| spread_all_modes_text(&text));

        let phony_config = ConfigLoadError::Error(IOError::other("hacky placeholder"));
        let global_parsed = Config::load(Ok(&global_config), Err(phony_config))?;

        // We need to build a transient `WorkspaceTrust` just to ask whether the workspace is
        // trusted enough to load its `.helix/config.toml`. The persisted-trust file on disk is the
        // source of truth either way; this transient instance has an empty cache and is dropped
        // after the check.
        let trust = helix_loader::workspace_trust::WorkspaceTrust::new(
            (&global_parsed.editor.workspace_trust).into(),
        );
        if trust
            .query_current(helix_loader::workspace_trust::TrustQuery::LocalConfig)
            .is_trusted()
        {
            let mut merged = Config::load(Ok(&global_config), local_config)?;
            // editor.workspace-trust is global/user-scope only. Without this override, a
            // workspace's `.helix/config.toml` could set `level = "insecure"`; once the user trusted
            // *that* workspace, refresh_config would re-load with the override merged in and from
            // then on every subsequent workspace in the session would be implicitly trusted. Pin
            // the gate's own configuration to the global file.
            merged.editor.workspace_trust = global_parsed.editor.workspace_trust;
            Ok(merged)
        } else {
            Ok(global_parsed)
        }
    }
}

/// sid's defaults, which the user's config.toml is laid over.
const DEFAULTS: &str = include_str!("defaults.toml");

/// The keys sid ships with, before your `config.toml` is laid over them: what a shortcut
/// goes back to when you give it back.
pub fn default_keys() -> HashMap<Mode, KeyTrie> {
    over_defaults("")
        .and_then(|text| Config::load(Ok(&text), Err(ConfigLoadError::default())))
        .map(|config| config.keys)
        .unwrap_or_else(|_| keymap::default())
}

/// The user's config.toml laid over sid's defaults, as the text `Config::load` reads.
pub(crate) fn over_defaults(user: &str) -> Result<String, ConfigLoadError> {
    let mut defaults: toml::Value = toml::from_str(DEFAULTS).expect("defaults.toml is valid TOML");
    spread_all_modes(&mut defaults);
    let mut user: toml::Value = toml::from_str(user).map_err(ConfigLoadError::BadConfig)?;
    spread_all_modes(&mut user);
    let merged = merge_toml_values(defaults, user, usize::MAX);
    Ok(toml::to_string(&merged).expect("a TOML value serializes"))
}

/// `[keys.all]` is laid under normal, select and insert, each mode's own table winning
/// over it: a key meant for every mode is written once.
fn spread_all_modes(config: &mut toml::Value) {
    let Some(keys) = config.get_mut("keys").and_then(toml::Value::as_table_mut) else {
        return;
    };
    let Some(all) = keys.remove("all") else {
        return;
    };
    for mode in ["normal", "select", "insert"] {
        let own = keys
            .remove(mode)
            .unwrap_or_else(|| toml::Value::Table(toml::Table::new()));
        let merged = merge_toml_values(all.clone(), own, usize::MAX);
        keys.insert(mode.to_owned(), merged);
    }
}

fn spread_all_modes_text(text: &str) -> Result<String, ConfigLoadError> {
    let mut config: toml::Value = toml::from_str(text).map_err(ConfigLoadError::BadConfig)?;
    spread_all_modes(&mut config);
    Ok(toml::to_string(&config).expect("a TOML value serializes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Config {
        fn load_test(config: &str) -> Config {
            Config::load(Ok(&config.to_owned()), Err(ConfigLoadError::default())).unwrap()
        }
    }

    #[test]
    fn parsing_keymaps_config_file() {
        use crate::keymap;
        use helix_core::hashmap;
        use helix_view::document::Mode;

        let sample_keymaps = r#"
            [keys.insert]
            y = "move_line_down"
            S-C-a = "delete_selection"

            [keys.normal]
            A-F12 = "move_next_word_end"
        "#;

        let mut keys = keymap::default();
        merge_keys(
            &mut keys,
            hashmap! {
                Mode::Insert => keymap!({ "Insert mode"
                    "y" => move_line_down,
                    "S-C-a" => delete_selection,
                }),
                Mode::Normal => keymap!({ "Normal mode"
                    "A-F12" => move_next_word_end,
                }),
            },
        );

        assert_eq!(
            Config::load_test(sample_keymaps),
            Config {
                keys,
                ..Default::default()
            }
        );
    }

    #[test]
    fn keys_resolve_to_correct_defaults() {
        // From serde default
        let default_keys = Config::load_test("").keys;
        assert_eq!(default_keys, keymap::default());

        // From the Default trait
        let default_keys = Config::default().keys;
        assert_eq!(default_keys, keymap::default());
    }

    fn command_at(config: &Config, mode: Mode, keys: &[&str]) -> String {
        let keys: Vec<_> = keys.iter().map(|key| key.parse().unwrap()).collect();
        match config.keys[&mode].search(&keys) {
            Some(KeyTrie::MappableCommand(command)) => command.name().to_owned(),
            other => panic!("{keys:?} is not one command: {other:?}"),
        }
    }

    #[test]
    fn with_no_config_file_the_defaults_are_sids() {
        let config = Config::load_test(&over_defaults("").unwrap());

        assert_eq!(config.editor.soft_wrap.enable, Some(false));
        assert!(!config.editor.file_picker.git_ignore);
        assert!(matches!(
            config.editor.bufferline,
            helix_view::editor::BufferLine::Always
        ));
        // Nothing is written for you: what is open and changed is asked about instead.
        assert!(!config.editor.auto_save.focus_lost);
        assert!(!config.editor.auto_save.after_delay.enable);
        assert!(matches!(config.theme, Some(theme::Config::Adaptive { .. })));
        assert_eq!(config.editor.default_mode, Mode::Insert);

        for mode in [Mode::Normal, Mode::Select, Mode::Insert] {
            assert_eq!(command_at(&config, mode, &["C-c"]), "copy_to_clipboard");
            assert_eq!(command_at(&config, mode, &["C-x"]), "cut_to_clipboard");
            assert_eq!(command_at(&config, mode, &["C-space"]), "completion");
            assert_eq!(command_at(&config, mode, &["C-v"]), "paste_from_clipboard");
            assert_eq!(command_at(&config, mode, &["C-z"]), "undo");
            assert_eq!(command_at(&config, mode, &["C-y"]), "redo");
            assert_eq!(command_at(&config, mode, &["C-p"]), "file_picker");
            assert_eq!(command_at(&config, mode, &["C-P"]), "command_palette");
            assert_eq!(command_at(&config, mode, &["F1"]), "command_palette");
            assert_eq!(command_at(&config, mode, &["S-F1"]), "keyboard_shortcuts");
            assert_eq!(command_at(&config, mode, &["C-e"]), "sidebar_focus");
            assert_eq!(command_at(&config, mode, &["C-r"]), "sidebar_reveal");
            // What the file tree does to the disk is reachable from every mode, so it
            // works whether the focus is in the tree or in the code.
            assert_eq!(command_at(&config, mode, &["C-A-n"]), "explorer_new");
            assert_eq!(command_at(&config, mode, &["C-A-r"]), "explorer_rename");
            assert_eq!(command_at(&config, mode, &["S-del"]), "explorer_delete");
            assert_eq!(command_at(&config, mode, &["A-R"]), "explorer_reveal");
            assert_eq!(
                command_at(&config, mode, &["C-o"]),
                "lsp_or_syntax_symbol_picker"
            );
            assert_eq!(
                command_at(&config, mode, &["C-l"]),
                "select_all_occurrences"
            );
            assert_eq!(command_at(&config, mode, &["F4"]), "review_context_toggle");
            assert_eq!(
                command_at(&config, mode, &["C-A-d"]),
                "review_side_by_side_toggle"
            );
            assert_eq!(command_at(&config, mode, &["F6"]), "review_cycle");
            // The way into modal editing has a key of its own in every mode, and the key
            // a terminal without the enhanced keyboard turns it into is the same thing.
            assert_eq!(command_at(&config, mode, &["C-A-m"]), "normal_mode");
            assert_eq!(command_at(&config, mode, &["A-ret"]), "normal_mode");
            assert_eq!(command_at(&config, mode, &["F7"]), "review_code_toggle");
            assert_eq!(command_at(&config, mode, &["F9"]), "review_files_toggle");
            assert_eq!(command_at(&config, mode, &["C-A-o"]), "outline_toggle");
            assert_eq!(command_at(&config, mode, &["C-F"]), "global_search");
            assert_eq!(command_at(&config, mode, &["C-b"]), "sidebar_toggle");
            assert_eq!(command_at(&config, mode, &["C-R"]), "sidebar_reveal");
            assert_eq!(command_at(&config, mode, &["S-F11"]), "sidebar_collapse");
            assert_eq!(command_at(&config, mode, &["S-F4"]), "buffer-close-all");
            assert_eq!(command_at(&config, mode, &["S-F5"]), "check-updates");
        }

        // Escape stays in insert: it closes what is open, it does not change the mode.
        assert_eq!(command_at(&config, Mode::Insert, &["esc"]), "escape");
        assert_eq!(
            command_at(&config, Mode::Insert, &["C-left"]),
            "move_prev_word_start"
        );
        assert_eq!(command_at(&config, Mode::Insert, &["C-u"]), "no_op");
        // What the language server answers is not normal mode's: the editor lives in
        // insert now, and a key only bound there would be dead where it is used.
        for mode in [Mode::Normal, Mode::Select, Mode::Insert] {
            assert_eq!(command_at(&config, mode, &["F12"]), "goto_definition");
            assert_eq!(command_at(&config, mode, &["S-F12"]), "goto_reference");
            assert_eq!(command_at(&config, mode, &["F2"]), "rename_symbol");
            assert_eq!(command_at(&config, mode, &["F8"]), "goto_next_diag");
        }
        assert_eq!(
            command_at(&config, Mode::Normal, &["space", "space"]),
            "global_search"
        );
        assert_eq!(
            command_at(&config, Mode::Normal, &["space", "/"]),
            "global_search"
        );

        for mode in [Mode::Normal, Mode::Select] {
            assert_eq!(command_at(&config, mode, &["C-a"]), "select_all");
            assert_eq!(
                command_at(&config, mode, &["backspace"]),
                "delete_selection_or_previous_char"
            );
            assert_eq!(
                command_at(&config, mode, &["del"]),
                "delete_selection_noyank"
            );
        }

        for mode in [Mode::Normal, Mode::Select, Mode::Insert] {
            assert_eq!(command_at(&config, mode, &["Cmd-c"]), "copy_to_clipboard");
            assert_eq!(
                command_at(&config, mode, &["C-d"]),
                "select_next_occurrence"
            );
            assert_eq!(command_at(&config, mode, &["C-K"]), "delete_line");
            assert_eq!(command_at(&config, mode, &["C-7"]), "toggle_comments");
            assert_eq!(command_at(&config, mode, &["C-/"]), "toggle_comments");
            assert_eq!(command_at(&config, mode, &["A-A"]), "toggle_block_comments");
            assert_eq!(command_at(&config, mode, &["C-w"]), "buffer-close");
            assert_eq!(command_at(&config, mode, &["C-S"]), "save_as");
            assert_eq!(command_at(&config, mode, &["C-s"]), "write");
            assert_eq!(command_at(&config, mode, &["C-q"]), "quit_saving");
            assert_eq!(command_at(&config, mode, &["C-,"]), "settings");
            assert_eq!(command_at(&config, mode, &["Cmd-s"]), "write");
            assert_eq!(command_at(&config, mode, &["C-n"]), "new");
            assert_eq!(command_at(&config, mode, &["C-g"]), "goto_line_prompt");
            assert_eq!(command_at(&config, mode, &["C-f"]), "search_in_file");
            assert_eq!(command_at(&config, mode, &["A-z"]), "toggle-option");
            assert_eq!(command_at(&config, mode, &["C-A-z"]), "toggle-option");
            assert_eq!(
                command_at(&config, mode, &["C-B"]),
                "markdown_preview_toggle"
            );
            assert_eq!(command_at(&config, mode, &["C-M"]), "markdown_preview_full");
        }
    }

    #[test]
    fn keys_all_reach_every_mode_and_a_modes_own_wins() {
        let user = r#"
[keys.all]
C-j = "move_line_down"
C-h = "move_char_left"

[keys.insert]
C-h = "no_op"
"#;
        let config = Config::load_test(&over_defaults(user).unwrap());

        for mode in [Mode::Normal, Mode::Select, Mode::Insert] {
            assert_eq!(command_at(&config, mode, &["C-j"]), "move_line_down");
        }
        assert_eq!(
            command_at(&config, Mode::Normal, &["C-h"]),
            "move_char_left"
        );
        assert_eq!(command_at(&config, Mode::Insert, &["C-h"]), "no_op");
        // What the defaults put under [keys.all] is still there beneath the user's.
        assert_eq!(command_at(&config, Mode::Insert, &["C-s"]), "write");
    }

    #[test]
    fn the_users_config_wins_over_the_defaults() {
        let user = r#"
            theme = "base16_default"

            [editor.soft-wrap]
            enable = false

            [keys.normal]
            C-c = "toggle_comments"

            [keys.normal.space]
            x = "file_picker"
        "#;
        let config = Config::load_test(&over_defaults(user).unwrap());

        assert_eq!(config.editor.soft_wrap.enable, Some(false));
        assert_eq!(
            config.theme,
            Some(theme::Config::Constant("base16_default".into()))
        );
        assert_eq!(
            command_at(&config, Mode::Normal, &["C-c"]),
            "toggle_comments"
        );
        assert_eq!(
            command_at(&config, Mode::Normal, &["space", "x"]),
            "file_picker"
        );
        // A key the user added under space leaves sid's others there.
        assert_eq!(
            command_at(&config, Mode::Normal, &["space", "space"]),
            "global_search"
        );
        // And what neither names stays sid's.
        assert!(!config.editor.file_picker.git_ignore);
        assert!(matches!(
            config.editor.bufferline,
            helix_view::editor::BufferLine::Always
        ));
    }

    #[test]
    fn a_broken_config_file_is_an_error_not_the_defaults() {
        assert!(matches!(
            over_defaults("[editor"),
            Err(ConfigLoadError::BadConfig(_))
        ));
    }
}
