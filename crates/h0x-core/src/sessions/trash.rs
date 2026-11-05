//! `session.trash`: a session's files, found as `session.list` finds them,
//! moved to the OS trash (Recycle Bin, Finder's Trash, the freedesktop
//! Trash). Never a permanent delete: where there is no trash, it refuses.
//! Only real files and folders inside the agent's own store are moved, never
//! a link, and the id is checked by its agent's shape first, so it can never
//! name a path.

use super::*;
use crate::rpc::RpcError;

/// Moves `agent`'s session `id` to the trash; the paths it moved.
pub fn trash(agent: &str, id: &str, roots: &Roots) -> Result<Vec<PathBuf>, RpcError> {
    super::resume::argv(agent, id).map_err(RpcError::invalid_params)?;
    let root = match agent {
        "claude" => &roots.claude,
        "codex" => &roots.codex,
        "opencode" => &roots.opencode,
        _ => return Err(unsupported(format!("h0x-core does not read {agent}'s sessions"))),
    };
    let found = match root {
        // Where the store's trash is decided before the store is read.
        Some(root) if trashable(root)? => match agent {
            "claude" => claude(&root.join("projects"), id),
            "codex" => codex(root, id),
            _ => opencode(root, id)?,
        },
        _ => Vec::new(),
    };
    if found.is_empty() {
        return Err(RpcError::new(
            "not_found",
            format!("no {agent} session {id} in its store"),
            "see `h0x-core session list`",
        ));
    }
    to_trash(&found, roots)?;
    Ok(found)
}

/// Whether `path` is a real file (or folder), not a link.
fn real(path: &Path, dir: bool) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| if dir { m.is_dir() } else { m.is_file() })
}

/// `<project>/<id>.jsonl` and, when Claude kept one, the `<project>/<id>/`
/// folder of its subagents and tool results, which only goes along with it.
fn claude(projects: &Path, id: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in dirs(projects) {
        let (session, side) = (dir.join(format!("{id}.jsonl")), dir.join(id));
        if real(&session, false) {
            found.push(session);
            if real(&side, true) {
                found.push(side);
            }
        }
    }
    found
}

fn codex(home: &Path, id: &str) -> Vec<PathBuf> {
    let name = format!("-{id}.jsonl");
    dirs(&home.join("sessions"))
        .flat_map(|year| dirs(&year))
        .flat_map(|month| dirs(&month))
        .flat_map(|day| files(&day, "jsonl"))
        .filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("rollout-") && n.ends_with(&name))
        })
        .collect()
}

/// The JSON storage's session file and its messages. A session in OpenCode's
/// database is rows, not files: it cannot go to the trash, and OpenCode's own
/// `session delete` deletes for good.
fn opencode(data: &Path, id: &str) -> Result<Vec<PathBuf>, RpcError> {
    let db = data.join("opencode.db");
    // A link or a folder in its place: what OpenCode holds there is unknown.
    if std::fs::symlink_metadata(&db).is_ok_and(|m| !m.is_file()) {
        return Err(unsupported(format!("{} is not a plain database file", db.display())));
    }
    if opencode::in_db(&db, id) {
        return Err(unsupported(format!(
            "OpenCode keeps session {id} in its database (opencode.db): it cannot be moved to the trash"
        )));
    }
    let storage = data.join("storage");
    let mut found: Vec<PathBuf> = dirs(&storage.join("session"))
        .map(|project| project.join(format!("{id}.json")))
        .filter(|p| real(p, false))
        .collect();
    let messages = storage.join("message").join(id);
    if !found.is_empty() && real(&messages, true) {
        found.push(messages);
    }
    Ok(found)
}

fn unsupported(message: String) -> RpcError {
    RpcError::new("unsupported", message, "remove it with the agent itself")
}

fn refused(message: String) -> RpcError {
    RpcError::new("unavailable", message, "nothing was deleted")
}

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

/// Whether a store can be read for the trash: only on a local disk, which has
/// a Recycle Bin. Asked of the drive letter alone, so a share, which has none,
/// is refused without being reached. A drive that is not there holds nothing.
#[cfg(windows)]
fn trashable(root: &Path) -> Result<bool, RpcError> {
    use std::path::{Component, Prefix};
    use windows_sys::Win32::Storage::FileSystem::GetDriveTypeW;
    // `DRIVE_NO_ROOT_DIR` and `DRIVE_FIXED` (Win32_System_WindowsProgramming).
    const NO_DRIVE: u32 = 1;
    const FIXED: u32 = 3;

    let drive = match root.components().next() {
        Some(Component::Prefix(p)) => match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => Some(d as char),
            _ => None,
        },
        _ => None,
    };
    // SAFETY: a NUL-terminated wide string that outlives the call.
    match drive.map(|d| unsafe { GetDriveTypeW(wide(&format!("{d}:\\")).as_ptr()) }) {
        Some(FIXED) => Ok(true),
        Some(NO_DRIVE) => Ok(false),
        _ => Err(unsupported(format!("{} is not on a local disk with a Recycle Bin", root.display()))),
    }
}

#[cfg(not(windows))]
fn trashable(_: &Path) -> Result<bool, RpcError> {
    Ok(true)
}

/// The Recycle Bin, through the shell, as Explorer's Delete does: undoable.
/// The paths are in a store [`trashable`] found on a local disk.
#[cfg(windows)]
fn to_trash(paths: &[PathBuf], _: &Roots) -> Result<(), RpcError> {
    use windows_sys::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FOF_WANTNUKEWARNING, SHFILEOPSTRUCTW,
        SHFileOperationW,
    };

    let mut from: Vec<u16> = Vec::new();
    for path in paths {
        // The shell takes plain `C:\...` paths, each NUL-terminated.
        let plain = path.display().to_string().replace('/', "\\");
        from.extend(wide(plain.strip_prefix(r"\\?\").unwrap_or(&plain)));
    }
    from.push(0);
    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        // A file the Recycle Bin cannot take asks rather than going for good.
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT | FOF_WANTNUKEWARNING) as u16,
        ..Default::default()
    };
    // SAFETY: `op` and the double-NUL-terminated `from` outlive the call.
    let code = unsafe { SHFileOperationW(&mut op) };
    if code != 0 || op.fAnyOperationsAborted != 0 || paths.iter().any(|p| std::fs::symlink_metadata(p).is_ok()) {
        return Err(refused(format!("the Recycle Bin did not take them (code {code:#x})")));
    }
    Ok(())
}

/// Finder's Trash, as Finder's Delete does (put back works).
#[cfg(target_os = "macos")]
fn to_trash(paths: &[PathBuf], _: &Roots) -> Result<(), RpcError> {
    // The paths are the script's arguments, never part of its text.
    const SCRIPT: [&str; 6] = [
        "on run paths",
        "repeat with p in paths",
        "set f to (POSIX file (contents of p)) as alias",
        "tell application \"Finder\" to delete f",
        "end repeat",
        "end run",
    ];
    let mut osascript = std::process::Command::new("/usr/bin/osascript");
    for line in SCRIPT {
        osascript.args(["-e", line]);
    }
    let out = osascript.args(paths).output().map_err(|e| refused(format!("osascript: {e}")))?;
    if !out.status.success() {
        return Err(refused(format!("Finder: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    Ok(())
}

/// The freedesktop home trash: `files/<name>` and `info/<name>.trashinfo`
/// (the Trash spec 1.0). Only a rename, so only on the Trash's file system.
#[cfg(all(unix, not(target_os = "macos")))]
fn to_trash(paths: &[PathBuf], roots: &Roots) -> Result<(), RpcError> {
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::DirBuilderExt;

    let Some(trash) = &roots.trash else { return Err(unsupported("no home folder, so no Trash".into())) };
    let (files, info) = (trash.join("files"), trash.join("info"));
    for dir in [&files, &info] {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| refused(format!("{}: {e}", dir.display())))?;
    }
    for path in paths {
        let absolute = std::path::absolute(path).map_err(|e| refused(format!("{}: {e}", path.display())))?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut encoded = String::new();
        for &b in absolute.as_os_str().as_bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => encoded.push(b as char),
                _ => encoded.push_str(&format!("%{b:02X}")),
            }
        }
        let entry = format!("[Trash Info]\nPath={encoded}\nDeletionDate={}\n", local_now());
        // The first free name: `name`, `name.2`, `name.3`, ...
        let mut n = 1;
        let (place, record) = loop {
            let candidate = if n == 1 { name.clone() } else { format!("{name}.{n}") };
            n += 1;
            let record = info.join(format!("{candidate}.trashinfo"));
            match std::fs::File::options().write(true).create_new(true).open(&record) {
                Ok(mut file) => {
                    let place = files.join(&candidate);
                    if std::fs::symlink_metadata(&place).is_ok() {
                        let _ = std::fs::remove_file(&record);
                        continue;
                    }
                    file.write_all(entry.as_bytes()).map_err(|e| refused(format!("{}: {e}", record.display())))?;
                    break (place, record);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(refused(format!("{}: {e}", record.display()))),
            }
        };
        if let Err(e) = std::fs::rename(path, &place) {
            let _ = std::fs::remove_file(&record);
            if e.raw_os_error() == Some(libc::EXDEV) {
                return Err(unsupported(format!("{} is not on the Trash's file system", path.display())));
            }
            return Err(refused(format!("{}: {e}", path.display())));
        }
    }
    Ok(())
}

/// Now as the Trash spec wants it: local time, `YYYY-MM-DDThh:mm:ss`.
#[cfg(all(unix, not(target_os = "macos")))]
fn local_now() -> String {
    // SAFETY: `time` and `localtime_r` only write the values passed to them.
    let tm = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}
