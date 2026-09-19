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
