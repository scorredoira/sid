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
            (before, format!("<esc>{keys}"), after, LineFeedHandling::AsIs),
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
