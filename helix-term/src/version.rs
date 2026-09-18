//! Which sid is running: the release it is, the commit it was built from, and when the
//! binary was written.

use std::time::UNIX_EPOCH;

/// The version with the time the binary was built at, when that can be read. Two builds
/// of the same commit — the usual thing while a change is being tried out — are told
/// apart by it, so a restart says at once whether the new sid is the one running.
pub fn describe() -> String {
    match built_at() {
        Some(at) => format!("{} built {at}", helix_loader::VERSION_AND_GIT_HASH),
        None => helix_loader::VERSION_AND_GIT_HASH.to_string(),
    }
}

/// When the binary being run was last written, by the clock of the machine running it.
/// An installed release carries the time it was built at in the archive.
pub fn built_at() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let built = exe.metadata().ok()?.modified().ok()?;
    let seconds = built.duration_since(UNIX_EPOCH).ok()?.as_secs();

    format_local(i64::try_from(seconds).ok()?)
}

/// The machine's own time, which is the time on the wall of whoever built it.
#[cfg(unix)]
fn format_local(seconds: i64) -> Option<String> {
    let time = seconds as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: `tm` is written only when the call returns a pointer to it.
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return None;
    }

    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    ))
}

#[cfg(not(unix))]
fn format_local(_seconds: i64) -> Option<String> {
    None
}
