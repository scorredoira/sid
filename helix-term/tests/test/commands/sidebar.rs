use super::*;
use helix_term::ui::sidebar::{
    changes::{Act, ChangesTab},
    git,
};
use helix_view::{doc, editor::Action};

#[tokio::test(flavor = "multi_thread")]
async fn discarding_an_open_untracked_file_closes_its_buffer() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("untracked.txt");
    let other = dir.path().join("other.txt");
    std::fs::write(&file, "discard me\n")?;
    std::fs::write(&other, "keep me\n")?;
    assert!(std::process::Command::new("git")
        .arg("init")
        .arg(dir.path())
        .output()?
        .status
        .success());
    let mut app = AppBuilder::new().with_file(&file, None).build()?;
    let discarded = doc!(app.editor).id();
    let kept = app.editor.open(&other, Action::Load)?;
    let change = git::status(dir.path())
        .unwrap()
        .into_iter()
        .find(|change| change.path == file)
        .unwrap();
    ChangesTab::new(dir.path().to_path_buf()).act(Act::Discard, change);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        helpers::run_event_loop_until_idle(&mut app),
    )
    .await?;
    assert!(!file.exists());
    assert!(app.editor.document(discarded).is_none());
    assert!(app.editor.document(kept).is_some());
    test_key_sequence(
        &mut app,
        Some(":wa<ret>"),
        Some(&|_| assert!(!file.exists())),
        false,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn discarding_a_tracked_file_reloads_its_buffer() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("tracked.txt");
    std::fs::write(&file, "committed\n")?;
    for args in [
        vec!["init"],
        vec!["add", "tracked.txt"],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-m",
            "fixture",
        ],
    ] {
        assert!(std::process::Command::new("git")
            .current_dir(dir.path())
            .args(args)
            .output()?
            .status
            .success());
    }
    std::fs::write(&file, "modified\n")?;
    let mut app = AppBuilder::new().with_file(&file, None).build()?;
    let id = doc!(app.editor).id();
    let change = git::status(dir.path()).unwrap().remove(0);
    ChangesTab::new(dir.path().to_path_buf()).act(Act::Discard, change);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        helpers::run_event_loop_until_idle(&mut app),
    )
    .await?;
    assert_eq!(
        app.editor.document(id).unwrap().text().to_string(),
        "committed\n"
    );
    assert_eq!(std::fs::read_to_string(&file)?, "committed\n");
    test_key_sequence(&mut app, Some("<esc>"), None, false).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn outline_follows_uncommitted_edits_and_undo() -> anyhow::Result<()> {
    use helix_core::Transaction;
    use helix_term::ui::sidebar::{entries::Row, outline::Outline, tab::TabView};
    use helix_view::{current, doc_mut};

    fn names(outline: &Outline) -> Vec<&str> {
        outline
            .rows()
            .iter()
            .filter_map(|row| match row {
                Row::Symbol(symbol) => Some(symbol.name.as_str()),
                _ => None,
            })
            .collect()
    }

    let dir = tempfile::tempdir()?;
    let file = dir.path().join("outline.rs");
    std::fs::write(&file, "fn alpha() {}\n")?;
    let mut app = AppBuilder::new().with_file(&file, None).build()?;
    assert!(
        doc!(app.editor).syntax().is_some(),
        "Rust grammar must be available"
    );
    let (mut outline, _) = Outline::new();
    // A file just switched to is read right away.
    outline.sync(&mut app.editor, false);
    assert_eq!(names(&outline), ["alpha"]);
    let revision = doc_mut!(app.editor).get_current_revision();
    {
        let (view, doc) = current!(app.editor);
        let change = Transaction::change(doc.text(), [(3, 8, Some("omega".into()))].into_iter());
        doc.apply(&change, view.id);
        assert_eq!(doc.get_current_revision(), revision);
        assert!(doc.is_modified());
    }
    // The same file edited waits for the typing to rest before it is read again.
    outline.sync(&mut app.editor, false);
    assert_eq!(names(&outline), ["alpha"]);
    outline.settle(&mut app.editor);
    assert_eq!(names(&outline), ["omega"]);
    assert_eq!(std::fs::read_to_string(&file)?, "fn alpha() {}\n");
    {
        let (view, doc) = current!(app.editor);
        doc.append_changes_to_history(view);
        assert!(doc.undo(view));
    }
    outline.sync(&mut app.editor, false);
    outline.settle(&mut app.editor);
    assert_eq!(names(&outline), ["alpha"]);
    {
        let (view, doc) = current!(app.editor);
        assert!(doc.redo(view));
    }
    outline.sync(&mut app.editor, false);
    outline.settle(&mut app.editor);
    assert_eq!(names(&outline), ["omega"]);
    test_key_sequence(&mut app, Some("<esc>"), None, false).await?;
    Ok(())
}
