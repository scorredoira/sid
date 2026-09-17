//! Keeping an installed sid on its latest release: `sid --update` and `:check-updates`.
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

use anyhow::{bail, Context};
use helix_loader::VERSION_AND_GIT_HASH;

const LATEST: &str = "https://github.com/scorredoira/sid/releases/latest";
const INSTALLER: &str = "https://raw.githubusercontent.com/scorredoira/sid/master/fork/install.sh";

/// How this sid was installed, which decides how it is brought up to date.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Install {
    /// By the installer, under `<prefix>/lib/sid/release.*`, linked from `<prefix>/bin`.
    Release { prefix: PathBuf },
    /// The single `.run` file, unpacked into the cache: the file itself is what to replace.
    Portable,
    /// A build of a clone, which `git pull` and `./build.sh` update.
    Source,
}

/// What asking for the latest release found.
pub struct Check {
    pub latest: String,
    pub newer: bool,
    pub install: Install,
}

/// The release a version names, and whether the build is past it: `v2026.9.17+2 (3c25a543)`
/// is two commits past `v2026.9.17`.
fn release(version: &str) -> Option<([u64; 3], bool)> {
    let version = version.split_whitespace().next()?.strip_prefix('v')?;
    let (numbers, past) = match version.split_once('+') {
        Some((numbers, _)) => (numbers, true),
        None => (version, false),
    };
    let mut parts = numbers.split('.').map(|part| part.parse().ok());
    let release = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some((release, past))
}

/// Whether `latest` is a release after the one `current` is or follows.
fn is_newer(latest: &str, current: &str) -> bool {
    match (release(latest), release(current)) {
        (Some((latest, _)), Some((current, _))) => latest > current,
        _ => false,
    }
}

fn install_of(exe: &Path) -> Install {
    let dir = exe.parent();
    let named = |path: Option<&Path>, name: &str| {
        path.and_then(Path::file_name)
            .is_some_and(|file| file.to_string_lossy() == name)
    };
    if let Some(dir) = dir {
        let release_dir = dir
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("release."));
        let sid = dir.parent();
        let lib = sid.and_then(Path::parent);
        if release_dir && named(sid, "sid") && named(lib, "lib") {
            if let Some(prefix) = lib.and_then(Path::parent) {
                return Install::Release {
                    prefix: prefix.to_path_buf(),
                };
            }
        }
    }
    let portable = exe
        .ancestors()
        .any(|dir| named(Some(dir), "portable") && named(dir.parent(), "sid"));
    if portable {
        Install::Portable
    } else {
        Install::Source
    }
}

/// How the running sid was installed, told by where its executable is.
pub fn install() -> Install {
    std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .map_or(Install::Source, |exe| install_of(&exe))
}

/// The tag of the latest release, read from where GitHub's "latest" link lands.
pub fn latest() -> anyhow::Result<String> {
    let output = Command::new("curl")
        .args([
            "-fsSLI",
            "-o",
            "/dev/null",
            "-w",
            "%{url_effective}",
            LATEST,
        ])
        .stderr(Stdio::piped())
        .output()
        .context("Could not run curl to ask for the latest release")?;
    if !output.status.success() {
        bail!(
            "Could not reach GitHub: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let url = String::from_utf8_lossy(&output.stdout);
    let tag = url.trim().rsplit('/').next().unwrap_or_default();
    if release(tag).is_none() {
        bail!("The latest release is not a version sid knows: {url}");
    }
    Ok(tag.to_string())
}

/// Asks for the latest release and whether it is newer than this build.
pub fn check() -> anyhow::Result<Check> {
    let latest = latest()?;
    Ok(Check {
        newer: is_newer(&latest, VERSION_AND_GIT_HASH),
        latest,
        install: install(),
    })
}

/// Why this install cannot update itself, and what does instead.
pub fn how_to_update(install: &Install) -> Option<String> {
    match install {
        Install::Release { .. } => None,
        Install::Portable => Some(format!(
            "This is the portable file: download the latest .run from {LATEST}"
        )),
        Install::Source => {
            Some("This sid was built from source: update it with git pull && ./build.sh".into())
        }
    }
}

/// Runs the installer for the latest release into `prefix`, beside what is installed: the
/// running sid goes on as it is, and the next start is the new one. `quiet` keeps what the
/// installer prints, for an error, instead of writing to a screen the editor owns.
pub fn install_latest(prefix: &Path, quiet: bool) -> anyhow::Result<()> {
    let script = r#"set -eu
installer=$(mktemp)
trap 'rm -f "$installer"' EXIT HUP INT TERM
curl -fsSL "$1" -o "$installer"
sh "$installer""#;
    let mut command = Command::new("sh");
    command
        .args(["-c", script, "sh", INSTALLER])
        .env("SID_PREFIX", prefix)
        .env_remove("SID_VERSION");
    if !quiet {
        let status = command.status().context("Could not run the installer")?;
        if !status.success() {
            bail!("The installer failed");
        }
        return Ok(());
    }
    let output = command
        .stdin(Stdio::null())
        .output()
        .context("Could not run the installer")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let reason = stderr.lines().last().unwrap_or("no reason given");
        bail!("The installer failed: {reason}");
    }
    Ok(())
}

/// The sid to start in place of this one once the editor has closed, when a restart into
/// an update was asked for.
static RESTART: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// The sid the installer put under `prefix`: the link the next start runs.
pub fn installed_binary(prefix: &Path) -> PathBuf {
    prefix.join("bin").join("sid")
}

/// Asks for `exe` to be started in place of this sid when the editor closes.
pub fn restart_into(exe: PathBuf) {
    *RESTART
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(exe);
}

/// The sid to start now that the editor has closed, if a restart was asked for.
pub fn take_restart() -> Option<PathBuf> {
    RESTART
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
}

/// Replaces this process with `exe`, given the same arguments and started from `dir`, the
/// directory this sid was started from. It returns only when that could not be done.
#[cfg(unix)]
pub fn restart(exe: &Path, dir: Option<&Path>) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    let mut command = Command::new(exe);
    command.args(std::env::args_os().skip(1));
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    command.exec()
}

#[cfg(not(unix))]
pub fn restart(exe: &Path, dir: Option<&Path>) -> std::io::Error {
    let mut command = Command::new(exe);
    command.args(std::env::args_os().skip(1));
    if let Some(dir) = dir {
        command.current_dir(dir);
    }
    match command.status() {
        Ok(status) => std::process::exit(status.code().unwrap_or(1)),
        Err(err) => err,
    }
}

/// `sid --update`: installs the latest release if it is newer, and says what happened.
pub fn run_from_command_line() -> anyhow::Result<i32> {
    let current = VERSION_AND_GIT_HASH;
    let install = install();
    if let Some(how) = how_to_update(&install) {
        eprintln!("{how}");
        return Ok(1);
    }
    let Install::Release { prefix } = install else {
        return Ok(1);
    };
    let latest = latest()?;
    if !is_newer(&latest, current) {
        println!("sid {current} is up to date (the latest release is {latest})");
        return Ok(0);
    }
    println!("Updating sid {current} to {latest}");
    install_latest(&prefix, false)?;
    println!("Updated to {latest}: restart sid to use it");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_is_newer_only_when_it_comes_after_this_build() {
        assert!(is_newer("v2026.9.19", "v2026.9.18 (e8abdc93)"));
        assert!(is_newer("v2026.10.1", "v2026.9.30"));
        assert!(!is_newer("v2026.9.19", "v2026.9.19 (1c6856c4)"));
        // Past a release is not behind it.
        assert!(!is_newer("v2026.9.19", "v2026.9.19+3 (abcdef12)"));
        assert!(is_newer("v2026.9.20", "v2026.9.19+3 (abcdef12)"));
        // A version sid cannot read never asks for an update.
        assert!(!is_newer("nightly", "v2026.9.19"));
        assert!(!is_newer("v2026.9.20", "25.07.1 (abcdef12)"));
    }

    #[test]
    fn where_the_executable_is_tells_how_it_was_installed() {
        assert_eq!(
            install_of(Path::new("/home/u/.local/lib/sid/release.AbC123/sid")),
            Install::Release {
                prefix: PathBuf::from("/home/u/.local")
            }
        );
        assert_eq!(
            install_of(Path::new("/home/u/.cache/sid/portable/0123abcd/sid")),
            Install::Portable
        );
        assert_eq!(
            install_of(Path::new("/home/u/projects/sid/target/release/sid")),
            Install::Source
        );
    }
}
