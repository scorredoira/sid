use super::*;
#[cfg(windows)]
use crossterm::event::{Event, KeyEvent};
use helix_core::hashmap;
use helix_term::keymap;
use helix_view::{doc, document::Mode, input::parse_macro};
#[cfg(not(windows))]
use termina::event::{Event, KeyEvent};

async fn key(app: &mut Application, keys: &str) -> anyhow::Result<()> {
    for key in parse_macro(keys)? {
        app.handle_terminal_event(Ok(Event::Key(KeyEvent::from(key))));
    }
    helpers::run_event_loop_until_idle(app).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn full_markdown_preview_blocks_terminal_paste() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("preview.md");
    let original = "# Heading\n";
    std::fs::write(&file, original)?;
    let mut config = helpers::test_config();
    config.editor.default_mode = Mode::Insert;
    config.keys.insert(
        Mode::Insert,
        keymap!({"Insert mode"
            "F11" => markdown_preview_full,
            "F12" => markdown_preview_toggle,
        }),
    );
    let mut app = AppBuilder::new()
        .with_config(config)
        .with_file(&file, None)
        .build()?;
    key(&mut app, "<F11>").await?;
    app.handle_terminal_event(Ok(Event::Paste("hidden edit".into())));
    assert_eq!(doc!(app.editor).text().to_string(), original);
    assert!(!doc!(app.editor).is_modified());
    key(&mut app, "<esc>").await?;
    app.handle_terminal_event(Ok(Event::Paste("visible edit".into())));
    assert_eq!(
        doc!(app.editor).text().to_string(),
        "visible edit# Heading\n"
    );
    key(&mut app, "<F12>").await?;
    app.handle_terminal_event(Ok(Event::Paste(" and side preview".into())));
    assert_eq!(
        doc!(app.editor).text().to_string(),
        "visible edit and side preview# Heading\n"
    );
    test_key_sequence(&mut app, Some("<esc>"), None, false).await?;
    Ok(())
}
