use super::*;
use helix_view::editor::{Action, BufferLine};

#[cfg(windows)]
use crossterm::event::{Event, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
#[cfg(not(windows))]
use termina::event::{Event, Modifiers as KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

fn tab_config() -> Config {
    let mut config = helpers::test_config();
    config.editor.bufferline = BufferLine::Always;
    config.editor.sidebar.open = false;
    config
}

async fn send_keys(app: &mut Application, keys: &str) -> anyhow::Result<()> {
    let mut events = tokio_stream::iter(
        helix_view::input::parse_macro(keys)?
            .into_iter()
            .map(|key| Ok(Event::Key(key.into()))),
    );
    assert!(app.event_loop_until_idle(&mut events).await);
    Ok(())
}

async fn click_close(app: &mut Application, name: &str) {
    let mut events = tokio_stream::iter([Ok(Event::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: name.len() as u16 + 4,
        row: 0,
        modifiers: KeyModifiers::empty(),
    }))]);
    assert!(app.event_loop_until_idle(&mut events).await);
}

async fn close_modified_background_tab(answer: &str) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let first = dir.path().join("first.txt");
    let second = dir.path().join("second.txt");
    std::fs::write(&first, "first\n")?;
    std::fs::write(&second, "second\n")?;
    let mut app = AppBuilder::new()
        .with_config(tab_config())
        .with_file(&first, None)
        .with_file(&second, None)
        .build()?;
    let target = app.editor.document_id_by_path(&first).unwrap();
    let other = app.editor.document_id_by_path(&second).unwrap();
    send_keys(&mut app, "ichanged <esc>").await?;
    app.editor.switch(other, Action::Replace);
    // Render the background tab's modified mark before clicking it.
    send_keys(&mut app, "<left>").await?;

    click_close(&mut app, "first.txt").await;
    assert!(app.editor.document(target).unwrap().is_modified());
    assert_eq!(std::fs::read_to_string(&first)?, "first\n");
    assert_eq!(helix_view::doc!(app.editor).id(), other);

    send_keys(&mut app, answer).await?;
    let cancelled = answer == "<esc>" || answer == "<tab><tab><ret>";
    assert_eq!(app.editor.document(target).is_some(), cancelled);
    assert!(app.editor.document(other).is_some());
    assert_eq!(std::fs::read_to_string(&second)?, "second\n");
    assert_eq!(
        std::fs::read_to_string(&first)?,
        if answer == "<ret>" {
            "changed first\n"
        } else {
            "first\n"
        },
    );
    if cancelled {
        assert_eq!(helix_view::doc!(app.editor).id(), other);
    }
    test_key_sequence(&mut app, Some("<esc>"), None, false).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_a_modified_background_tab_can_be_cancelled_with_escape() -> anyhow::Result<()> {
    close_modified_background_tab("<esc>").await
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_a_modified_background_tab_can_be_cancelled() -> anyhow::Result<()> {
    close_modified_background_tab("<tab><tab><ret>").await
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_a_modified_background_tab_can_discard_only_its_changes() -> anyhow::Result<()> {
    close_modified_background_tab("<tab><ret>").await
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_a_modified_background_tab_can_save_only_its_changes() -> anyhow::Result<()> {
    close_modified_background_tab("<ret>").await
}

#[tokio::test(flavor = "multi_thread")]
async fn closing_an_unnamed_tab_can_save_it_before_closing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let saved = dir.path().join("saved.txt");
    let mut app = AppBuilder::new().with_config(tab_config()).build()?;
    send_keys(&mut app, "ikeep this<esc>").await?;
    let target = helix_view::doc!(app.editor).id();
    let name = helix_view::doc!(app.editor).scratch_name().to_string();

    click_close(&mut app, &name).await;
    // Save and close first asks for the unnamed buffer's destination.
    send_keys(&mut app, "<ret>").await?;
    assert!(app.editor.document(target).unwrap().is_modified());
    assert!(!saved.exists());
    send_keys(&mut app, &format!("{}<ret>", saved.display())).await?;
    assert_eq!(std::fs::read_to_string(saved)?, "keep this\n");
    assert!(app.editor.document(target).is_none());
    test_key_sequence(&mut app, Some("<esc>"), None, false).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn failing_to_save_a_tab_keeps_its_changes_open() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("first.txt");
    std::fs::write(&file, "first\n")?;
    let mut app = AppBuilder::new()
        .with_config(tab_config())
        .with_file(&file, None)
        .build()?;
    send_keys(&mut app, "ichanged <esc>").await?;
    let target = helix_view::doc!(app.editor).id();
    // A directory cannot be overwritten with the buffer's text.
    std::fs::remove_file(&file)?;
    std::fs::create_dir(&file)?;
    click_close(&mut app, "first.txt").await;
    send_keys(&mut app, "<ret>").await?;
    let doc = app.editor.document(target).unwrap();
    assert!(doc.is_modified());
    assert_eq!(doc.text().to_string(), "changed first\n");
    assert!(file.is_dir());
    test_key_sequence(&mut app, Some("<esc>"), None, false).await?;
    Ok(())
}
