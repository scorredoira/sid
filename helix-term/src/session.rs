//! What a project had open, so that opening the editor on it again opens the same tabs.

use std::path::{Path, PathBuf};

use anyhow::Context as _;

/// The files a project had open, in the order their tabs were in, and how the screen was
/// split between them.
#[derive(Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Session {
    pub files: Vec<PathBuf>,
    /// The one that was in front.
    pub focused: Option<PathBuf>,
    /// The splits, down to a file and a cursor in each; none when nothing with a file was
    /// on screen.
    pub layout: Option<Pane>,
}

/// A split and what it holds, or one view: a file and where the cursor was in it. Untagged,
/// so the file reads as `{ file = "a.rs", line = 3, column = 0 }` and a split as
/// `{ split = "vertical", panes = [...] }`.
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum Pane {
    Split {
        split: Split,
        /// Each pane's share of the split, in the order of `panes`, so a split dragged to
        /// 70/30 does not come back 50/50; left out when the panes share it evenly, and
        /// a session written before there were sizes reads as evenly shared.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        sizes: Vec<u32>,
        panes: Vec<Pane>,
    },
    View {
        file: PathBuf,
        line: usize,
        column: usize,
        /// The first line on screen, so the file comes back scrolled where it was; a
        /// session written before it was kept centres the cursor instead.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        top: Option<usize>,
    },
}

#[derive(Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Split {
    Vertical,
    Horizontal,
}

fn sessions_dir() -> PathBuf {
    helix_loader::data_dir().join("sessions")
}

/// A stable name for a project: its own folder's, and a hash of the whole path, so that two
/// projects whose folders are called the same are not one session.
fn key(workspace: &Path) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in workspace.as_os_str().as_encoded_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }

    let name = workspace
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");

    format!("{name}-{hash:016x}")
}

fn session_file(workspace: &Path) -> PathBuf {
    sessions_dir().join(format!("{}.toml", key(workspace)))
}

/// What this project had open last; none before it ever had any.
pub fn load(workspace: &Path) -> anyhow::Result<Option<Session>> {
    let path = session_file(workspace);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
    };

    let session: Session =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

    Ok(Some(session))
}

/// Written aside and renamed over, so another editor reading it never sees half.
pub fn save(workspace: &Path, session: &Session) -> anyhow::Result<()> {
    let dir = sessions_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let path = session_file(workspace);
    let text = toml::to_string(session)?;
    let temp = dir.join(format!(".{}.{}.toml", key(workspace), std::process::id()));
    std::fs::write(&temp, text).with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, &path).with_context(|| format!("replacing {}", path.display()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_with_splits_reads_back_as_written() {
        let session = Session {
            files: vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")],
            focused: Some(PathBuf::from("b.rs")),
            layout: Some(Pane::Split {
                split: Split::Vertical,
                sizes: vec![7, 3],
                panes: vec![
                    Pane::View {
                        file: PathBuf::from("a.rs"),
                        line: 3,
                        column: 1,
                        top: Some(2),
                    },
                    Pane::Split {
                        split: Split::Horizontal,
                        sizes: vec![],
                        panes: vec![
                            Pane::View {
                                file: PathBuf::from("b.rs"),
                                line: 0,
                                column: 0,
                                top: None,
                            },
                            Pane::View {
                                file: PathBuf::from("a.rs"),
                                line: 9,
                                column: 4,
                                top: Some(9),
                            },
                        ],
                    },
                ],
            }),
        };

        let text = toml::to_string(&session).unwrap();
        let back: Session = toml::from_str(&text).unwrap();

        assert!(back == session);
        // Even shares and an unknown scroll are left out rather than written as zeros.
        assert_eq!(text.matches("sizes").count(), 1);
        assert_eq!(text.matches("top").count(), 2);
    }

    #[test]
    fn a_session_written_before_sizes_and_scroll_still_reads() {
        let text = "files = [\"a.rs\"]\n\n[layout]\nsplit = \"vertical\"\n\n\
            [[layout.panes]]\nfile = \"a.rs\"\nline = 3\ncolumn = 1\n\n\
            [[layout.panes]]\nfile = \"b.rs\"\nline = 0\ncolumn = 0\n";
        let session: Session = toml::from_str(text).unwrap();
        let Some(Pane::Split { sizes, panes, .. }) = session.layout else {
            panic!("a split");
        };
        assert!(sizes.is_empty());
        assert!(matches!(
            &panes[0],
            Pane::View {
                top: None,
                line: 3,
                ..
            }
        ));
    }

    #[test]
    fn two_projects_called_the_same_are_two_sessions() {
        let one = key(Path::new("/home/someone/work/site"));
        let two = key(Path::new("/home/someone/play/site"));

        assert!(one.starts_with("site-"));
        assert!(two.starts_with("site-"));
        assert_ne!(one, two);

        // And the same project is the same session, run after run.
        assert_eq!(one, key(Path::new("/home/someone/work/site")));
    }
}
