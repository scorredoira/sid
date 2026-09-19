use helix_core::hashmap;
use helix_term::keymap;
use helix_view::document::Mode;

use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn moving_lines_preserves_the_final_line_ending() -> anyhow::Result<()> {
    for (before, keys, after) in [
        ("#[a|]#lpha\nbeta", "<C-down>", "beta\n#[a|]#lpha"),
        ("alpha\n#[b|]#eta", "<C-up>", "#[b|]#eta\nalpha"),
        ("#[a|]#lpha\nbeta\n", "<C-down>", "beta\n#[a|]#lpha\n"),
        ("#[a|]#lpha\r\nbeta", "<C-down>", "beta\r\n#[a|]#lpha"),
        ("#[a|]#lpha\nbeta", "<C-down>u", "#[a|]#lpha\nbeta"),
    ] {
        let mut config = helpers::test_config();
        config.editor.default_line_ending = if before.contains("\r\n") {
            helix_view::editor::LineEndingConfig::Crlf
        } else {
            helix_view::editor::LineEndingConfig::LF
        };
        config.keys.insert(
            Mode::Normal,
            keymap!({"Normal mode"
                "C-up" => move_lines_up,
                "C-down" => move_lines_down,
            }),
        );
        test_with_config(
            AppBuilder::new().with_config(config),
            (
                before,
                format!("<esc>{keys}"),
                after,
                LineFeedHandling::AsIs,
            ),
        )
        .await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn moving_lines_with_several_cursors_moves_each_block_on_its_own() -> anyhow::Result<()> {
    for (before, keys, after) in [
        // Two lines apart each swap with their own neighbour; the line between stays.
        (
            "#[a|]#\nb\n#(c|)#\nd\n",
            "<C-down>",
            "b\n#[a|]#\nd\n#(c|)#\n",
        ),
        ("a\n#[b|]#\nc\n#(d|)#\n", "<C-up>", "#[b|]#\na\n#(d|)#\nc\n"),
        // Touching lines are one block.
        (
            "a\n#[b|]#\n#(c|)#\nd\n",
            "<C-down>",
            "a\nd\n#[b|]#\n#(c|)#\n",
        ),
        // One block with nowhere to go holds the others.
        ("#[a|]#\nb\n#(c|)#\n", "<C-up>", "#[a|]#\nb\n#(c|)#\n"),
        ("a\n#[b|]#\nc\n#(d|)#", "<C-down>", "a\n#[b|]#\nc\n#(d|)#"),
        // The last line has no ending, and keeps it that way.
        ("#[a|]#\nb\n#(c|)#\nd", "<C-down>", "b\n#[a|]#\nd\n#(c|)#"),
    ] {
        let mut config = helpers::test_config();
        config.keys.insert(
            Mode::Normal,
            keymap!({"Normal mode"
                "C-up" => move_lines_up,
                "C-down" => move_lines_down,
            }),
        );
        test_with_config(
            AppBuilder::new().with_config(config),
            (
                before,
                format!("<esc>{keys}"),
                after,
                LineFeedHandling::AsIs,
            ),
        )
        .await?;
    }
    Ok(())
}

fn with_shortcuts() -> AppBuilder {
    let mut config = helpers::test_config();
    config.keys.insert(
        Mode::Normal,
        keymap!({"Normal mode"
            "C-g" => goto_line_prompt,
            "C-f" => search_in_file,
            "C-q" => quit_saving,
            "backspace" => delete_selection_or_previous_char,
        }),
    );

    config.keys.insert(
        Mode::Insert,
        keymap!({"Insert mode"
            "C-a" => select_all,
            "S-right" => extend_char_right,
            "F10" => command_mode,
        }),
    );

    AppBuilder::new().with_config(config)
}

/// The editor as installed: it opens in insert mode and a buffer switch leaves it there.
fn typing_by_default() -> AppBuilder {
    let mut config = helpers::test_config();
    config.editor.default_mode = Mode::Insert;
    config.keys.insert(
        Mode::Insert,
        keymap!({"Insert mode"
            "S-right" => extend_char_right,
            "F10" => command_mode,
        }),
    );

    AppBuilder::new().with_config(config)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_selection_made_while_typing_stays_in_its_buffer() -> anyhow::Result<()> {
    let mut app = typing_by_default().with_input_text("#[a|]#bcd").build()?;

    // A second buffer with a line selected the normal way, then back to the first one
    // to select while typing, then to the second again, still typing. What is typed
    // there lands beside its selection: the selection typing replaces was the first
    // buffer's, never this one's.
    test_key_sequences(
        &mut app,
        vec![(
            Some("<F10>new<ret>hello<esc>x:bp<ret>i<S-right><S-right><F10>bn<ret>Z"),
            Some(&|app| {
                let text = helix_view::doc!(app.editor).text().to_string();
                assert_eq!("helloZ\n", text);
            }),
        )],
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_selection_made_while_typing_is_replaced_by_typing() -> anyhow::Result<()> {
    test_with_config(
        typing_by_default(),
        ("#[a|]#bcd", "<S-right><S-right>X", "X#[c|]#d"),
    )
    .await?;

    Ok(())
}

/// Shift-F1 end to end: a key given there is written to config.toml and runs at once;
/// taken away from the row's menu it is gone; given back to sid, sid's own key runs
/// again and nothing of ours is left in the file; and Ctrl-Z on the screen puts the
/// file back as it was before the last change made there.
#[tokio::test(flavor = "multi_thread")]
async fn the_shortcuts_screen_writes_config_toml_and_the_editor_runs_what_it_wrote(
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    helix_loader::initialize_config_file(Some(directory.path().join("config.toml")));
    let path = helix_loader::config_file();
    let written = || std::fs::read_to_string(&path).unwrap_or_default();

    let mut config = helpers::test_config();
    config.keys = helix_term::config::default_keys();
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_input_text("#[a|]#bc")
        .build()?;

    test_key_sequences(
        &mut app,
        vec![
            // The action found by name, Enter, the keys, Enter: the line is written under
            // the modal tables, and the key it had is left doing nothing.
            (
                Some("<S-F1>switch_case<ret><C-A-j><ret><esc>"),
                Some(&|_| {
                    let written = written();
                    assert!(
                        written.contains("[keys.normal]\nA-C-j = \"switch_case\""),
                        "{written}"
                    );
                    assert!(
                        written.contains("[keys.select]\nA-C-j = \"switch_case\""),
                        "{written}"
                    );
                    assert!(written.contains("\"~\" = \"no_op\""), "{written}");
                }),
            ),
            // And the editor runs it without being restarted.
            (
                Some("<C-A-j>"),
                Some(&|app| {
                    assert_eq!(helix_view::doc!(app.editor).text().to_string(), "Abc");
                }),
            ),
            // Taken away from the row's menu: our line is gone, sid's key stays off.
            (
                Some("<S-F1>switch_case<S-F10><down><ret><esc>"),
                Some(&|_| {
                    let written = written();
                    assert!(!written.contains("switch_case"), "{written}");
                    assert!(written.contains("\"~\" = \"no_op\""), "{written}");
                }),
            ),
            (
                Some("<C-A-j>"),
                Some(&|app| {
                    assert_eq!(helix_view::doc!(app.editor).text().to_string(), "Abc");
                }),
            ),
            // Given back to sid by what it runs, keys or no keys: nothing of ours is left.
            (
                Some("<S-F1>switch_case<S-F10><down><ret><esc>"),
                Some(&|_| {
                    let written = written();
                    assert!(!written.contains("keys"), "{written}");
                }),
            ),
            (
                Some("~"),
                Some(&|app| {
                    assert_eq!(helix_view::doc!(app.editor).text().to_string(), "abc");
                }),
            ),
            // A change made on the screen is undone there with Ctrl-Z.
            (
                Some("<S-F1>switch_case<ret><C-A-k><ret><C-z><esc>"),
                Some(&|_| {
                    let written = written();
                    assert!(!written.contains("switch_case"), "{written}");
                    assert!(!written.contains("no_op"), "{written}");
                }),
            ),
            (
                Some("~"),
                Some(&|app| {
                    assert_eq!(helix_view::doc!(app.editor).text().to_string(), "Abc");
                }),
            ),
        ],
        false,
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cmd_key_nobody_bound_writes_nothing() -> anyhow::Result<()> {
    test(("#[a|]#", "i<Cmd-c>x<C-b>y<esc>", "xy#[|a]#")).await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn go_to_line_asks_for_the_number() -> anyhow::Result<()> {
    test_with_config(
        with_shortcuts(),
        (
            "#[o|]#ne\ntwo\nthree\nfour\n",
            "<C-g>3<ret>",
            "one\ntwo\n#[t|]#hree\nfour\n",
        ),
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn go_to_line_puts_the_cursor_back_on_escape() -> anyhow::Result<()> {
    test_with_config(
        with_shortcuts(),
        (
            "one\n#[t|]#wo\nthree\nfour\n",
            "<C-g>4<esc>",
            "one\n#[t|]#wo\nthree\nfour\n",
        ),
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn go_to_line_refuses_what_is_not_a_number() -> anyhow::Result<()> {
    test_with_config(
        with_shortcuts(),
        (
            "one\n#[t|]#wo\nthree\n",
            "<C-g>3x<ret>",
            "one\n#[t|]#wo\nthree\n",
        ),
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn search_in_file_replaces_in_this_buffer() -> anyhow::Result<()> {
    test_with_config(
        with_shortcuts(),
        (
            "#[l|]#et tenant = 1;\nlet other = tenant;\n",
            "<C-f>tenant<A-h><tab>account<A-a><esc>",
            "#[l|]#et account = 1;\nlet other = account;\n",
        ),
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn backspace_takes_the_character_before_a_bare_cursor() -> anyhow::Result<()> {
    test_with_config(
        with_shortcuts(),
        ("hello #[w|]#orld", "<backspace>", "hello#[w|]#orld"),
    )
    .await?;

    // At the very start there is nothing before it.
    test_with_config(
        with_shortcuts(),
        ("#[h|]#ello", "<backspace>", "#[h|]#ello"),
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn backspace_deletes_what_is_selected() -> anyhow::Result<()> {
    test_with_config(
        with_shortcuts(),
        ("#[hello|]# world", "<backspace>", "#[ |]#world"),
    )
    .await?;

    // A selection and a cursor right after it: the cursor does not reach into it.
    test_with_config(
        with_shortcuts(),
        ("#[ab|]##(c|)#d", "<backspace>", "#[c|]#d"),
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn typing_over_a_selection_replaces_it() -> anyhow::Result<()> {
    test_with_config(
        with_shortcuts(),
        ("#[a|]#bcd", "i<S-right><S-right>X", "X#[c|]#d"),
    )
    .await?;

    // Everything selected, the line feed included, and one letter left.
    test_with_config(
        with_shortcuts(),
        ("#[a|]#bcd\n", "i<C-a>X", "X#[|]#", LineFeedHandling::AsIs),
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn backspace_while_typing_takes_the_selection_whole() -> anyhow::Result<()> {
    test_with_config(
        with_shortcuts(),
        ("#[a|]#bcd", "i<S-right><S-right><backspace>", "#[c|]#d"),
    )
    .await?;

    test_with_config(
        with_shortcuts(),
        ("#[a|]#bcd", "i<S-right><S-right><del>", "#[c|]#d"),
    )
    .await?;

    // And once it is gone, Backspace goes back to taking the character before the
    // cursor, never the one after it.
    test_with_config(
        with_shortcuts(),
        (
            "#[a|]#bcd",
            "i<right><right><S-right><backspace><backspace>",
            "a#[d|]#",
        ),
    )
    .await?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn leaving_a_file_saves_it() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let other = tempfile::NamedTempFile::new()?;
    let mut config = helpers::test_config();
    config.editor.auto_save.focus_lost = true;

    let mut app = helpers::AppBuilder::new()
        .with_config(config)
        .with_file(file.path(), None)
        .build()?;

    // Typed into the first file, then away to the second one.
    test_key_sequence(
        &mut app,
        Some(&format!(
            "ihello<esc>:open {}<ret>",
            other.path().to_string_lossy()
        )),
        None,
        false,
    )
    .await?;

    helpers::run_event_loop_until_idle(&mut app).await;
    helpers::assert_file_has_content(&mut file, &LineFeedHandling::Native.apply("hello\n"))?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn delayed_save_keeps_working_while_typing_in_insert_mode() -> anyhow::Result<()> {
    let file = helpers::temp_file_with_contents("")?;
    let mut config = helpers::test_config();
    config.editor.default_mode = Mode::Insert;
    config.editor.auto_save.after_delay.enable = true;
    config.editor.auto_save.after_delay.timeout = 25;
    config
        .keys
        .insert(Mode::Insert, keymap!({"Insert mode" "C-z" => undo, }));
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(file.path(), None)
        .build()?;
    let check = |expected: &str, app: &helix_term::application::Application| {
        assert_eq!(app.editor.mode(), Mode::Insert);
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), expected);
        assert!(!helix_view::doc!(app.editor).is_modified());
    };
    test_key_sequences(
        &mut app,
        vec![
            (Some("hello"), Some(&|app| check("hello\n", app))),
            (Some(" world"), Some(&|app| check("hello world\n", app))),
            (Some("<C-z>"), Some(&|app| check("hello\n", app))),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn delayed_save_leaves_the_space_just_typed_when_trimming_whitespace() -> anyhow::Result<()> {
    let file = helpers::temp_file_with_contents("")?;
    let mut config = helpers::test_config();
    config.editor.default_mode = Mode::Insert;
    config.editor.auto_save.after_delay.enable = true;
    config.editor.auto_save.after_delay.timeout = 25;
    config.editor.trim_trailing_whitespace = true;
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(file.path(), None)
        .build()?;
    let check = |expected: &str, app: &helix_term::application::Application| {
        assert_eq!(app.editor.mode(), Mode::Insert);
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), expected);
        assert!(!helix_view::doc!(app.editor).is_modified());
    };
    test_key_sequences(
        &mut app,
        vec![
            // The caret's own line keeps its trailing space: the next word needs it.
            (Some("hello "), Some(&|app| check("hello \n", app))),
            (Some("world"), Some(&|app| check("hello world\n", app))),
            // A line the caret has left is trimmed on the next save.
            (
                Some(" <ret>next"),
                Some(&|app| check("hello world\nnext\n", app)),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn delayed_save_keeps_the_blank_lines_the_caret_opened_at_the_end() -> anyhow::Result<()> {
    let file = helpers::temp_file_with_contents("")?;
    let mut config = helpers::test_config();
    config.editor.default_mode = Mode::Insert;
    config.editor.auto_save.after_delay.enable = true;
    config.editor.auto_save.after_delay.timeout = 25;
    config.editor.trim_final_newlines = true;
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(file.path(), None)
        .build()?;
    let check = |expected: &str, app: &helix_term::application::Application| {
        assert_eq!(app.editor.mode(), Mode::Insert);
        assert_eq!(std::fs::read_to_string(file.path()).unwrap(), expected);
        assert!(!helix_view::doc!(app.editor).is_modified());
    };
    test_key_sequences(
        &mut app,
        vec![
            // Two Enters at the end and a pause: the caret stays on its new line.
            (
                Some("hello<ret><ret>"),
                Some(&|app| check("hello\n\n", app)),
            ),
            (Some("world"), Some(&|app| check("hello\n\nworld\n", app))),
            // Once the caret is back up, the extra newlines go.
            (
                Some("<ret><ret><up><up>"),
                Some(&|app| check("hello\n\nworld\n", app)),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn delayed_save_writes_the_file_that_changed_and_no_other() -> anyhow::Result<()> {
    let first = helpers::temp_file_with_contents("first\n")?;
    let second = helpers::temp_file_with_contents("second\n")?;
    let mut config = helpers::test_config();
    config.editor.default_mode = Mode::Insert;
    config.editor.auto_save.after_delay.timeout = 25;
    config.keys.insert(
        Mode::Insert,
        keymap!({"Insert mode" "F10" => command_mode, }),
    );
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(first.path(), None)
        .build()?;
    let open_second = format!("<F10>o {}<ret>", second.path().display());
    test_key_sequences(
        &mut app,
        vec![
            // The first file is changed before delayed saves are on, so it stays
            // modified; then the second is changed with them on.
            (Some("x"), None),
            (
                Some("<F10>set auto-save.after-delay.enable true<ret>"),
                None,
            ),
            (
                Some(&open_second),
                Some(&|app| {
                    assert_eq!(app.editor.documents().count(), 2);
                }),
            ),
            (
                Some("y"),
                Some(&|app| {
                    assert_eq!(std::fs::read_to_string(second.path()).unwrap(), "ysecond\n");
                    assert_eq!(std::fs::read_to_string(first.path()).unwrap(), "first\n");
                    let unsaved: Vec<_> = app
                        .editor
                        .documents()
                        .filter(|doc| doc.is_modified())
                        .map(|doc| doc.path().unwrap().to_path_buf())
                        .collect();
                    assert_eq!(unsaved, vec![first.path().to_path_buf()]);
                }),
            ),
        ],
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn quitting_writes_every_file_after_confirmation() -> anyhow::Result<()> {
    let mut file = tempfile::NamedTempFile::new()?;
    let mut app = with_shortcuts().with_file(file.path(), None).build()?;

    test_key_sequences(
        &mut app,
        vec![
            (
                Some("ihello<esc><C-q>"),
                Some(&|_| {
                    assert_eq!(std::fs::read_to_string(file.path()).unwrap(), "");
                }),
            ),
            (Some("<ret>"), None),
        ],
        true,
    )
    .await?;

    helpers::assert_file_has_content(&mut file, &LineFeedHandling::Native.apply("hello\n"))?;

    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn quitting_asks_about_what_cannot_be_written() -> anyhow::Result<()> {
    // A buffer with no file behind it: quitting would lose it, so the editor stays.
    let mut app = with_shortcuts().build()?;

    test_key_sequence(&mut app, Some("ihello<esc><C-q>"), None, false).await?;

    Ok(())
}
