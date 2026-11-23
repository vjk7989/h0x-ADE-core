//! The first Linux SSH start needs the same exported settings as a login.
//! Capture outside the protocol and before systemd removes the SSH session.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CAPTURING: &str = "H0X_CORE_LOGIN_CAPTURE";
const MARKER: &[u8] = b"\0H0X_LOGIN_ENV\0";
const END: &[u8] = b"\0H0X_LOGIN_END\0";
const SCRIPT: &str = "printf '\\0H0X_LOGIN_ENV\\0'; /usr/bin/env -0; printf '\\0H0X_LOGIN_END\\0'";

pub(super) fn apply(cmd: &mut Command, state: &Path) {
    if std::env::var_os(CAPTURING).is_some() {
        cmd.env_remove(CAPTURING);
        return;
    }
    if std::env::var_os(crate::sweep::RUNTIME).is_some()
        || !super::SSH_LOGIN.iter().any(|key| std::env::var_os(key).is_some())
    {
        return;
    }
    let shell = std::env::var_os("SHELL").filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/sh".into());
    let result = capture(&shell, state);
    match result {
        Ok(mut vars) => {
            vars.remove(std::ffi::OsStr::new(CAPTURING));
            let mut vars = super::restored_environment(vars, std::env::vars_os());
            // Profiles must not hide the SSH origin from systemd's session filtering.
            for key in super::SSH_LOGIN {
                if let Some(value) = std::env::var_os(key) {
                    vars.insert((*key).into(), value);
                }
            }
            for (key, _) in std::env::vars_os() {
                if !vars.contains_key(&key) {
                    cmd.env_remove(key);
                }
            }
            cmd.envs(vars);
        }
        Err(error) => {
            if let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(state.join("core.log")) {
                let _ = writeln!(
                    log,
                    "core: login environment from {} unavailable: {error}; using inherited environment",
                    Path::new(&shell).display()
                );
            }
        }
    }
}

fn capture(shell: &std::ffi::OsStr, state: &Path) -> io::Result<BTreeMap<OsString, OsString>> {
    let mut cmd = Command::new(shell);
    cmd.env(CAPTURING, "1");
    let name = Path::new(shell).file_name().unwrap_or_default().to_string_lossy();
    match name.as_ref() {
        "csh" | "tcsh" => {
            cmd.arg0(format!("-{name}"));
        }
        _ => {
            cmd.arg("-l");
        }
    }
    cmd.args(["-c", SCRIPT]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut bytes = Vec::new();
    let mut diagnostics = Vec::new();
    let result = (|| {
        for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
            if unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) } < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        let start = Instant::now();
        let mut remaining = 1 << 20;
        loop {
            let status = child.try_wait()?;
            for (stream, output) in
                [(&mut stdout as &mut dyn Read, &mut bytes), (&mut stderr as &mut dyn Read, &mut diagnostics)]
            {
                let mut buffer = [0; 8192];
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            if n > remaining {
                                return Err(io::Error::other("profile output exceeded 1 MiB"));
                            }
                            remaining -= n;
                            output.extend_from_slice(&buffer[..n]);
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => return Err(error),
                    }
                    if start.elapsed() >= Duration::from_secs(5) {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "profile exceeded 5 s"));
                    }
                }
            }
            if let Some(status) = status {
                if !status.success() {
                    return Err(io::Error::other(format!("profile exited with {status}")));
                }
                return parse(&bytes);
            }
            if start.elapsed() >= Duration::from_secs(5) {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "profile exceeded 5 s"));
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    })();
    // The shell may exit before its background jobs close the capture pipes.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.wait();
    if let Ok(mut log) = std::fs::OpenOptions::new().create(true).append(true).open(state.join("core.log")) {
        let _ = log.write_all(&diagnostics[..diagnostics.len().min(1 << 20)]);
    }
    result
}

fn parse(bytes: &[u8]) -> io::Result<BTreeMap<OsString, OsString>> {
    let invalid = || io::Error::other("profile did not produce a complete environment");
    let start = bytes.windows(MARKER.len()).position(|part| part == MARKER).ok_or_else(invalid)? + MARKER.len();
    let tail = &bytes[start..];
    let end = tail.windows(END.len()).position(|part| part == END).ok_or_else(invalid)?;
    let bytes = &tail[..end];
    if bytes.is_empty() || !bytes.ends_with(&[0]) {
        return Err(invalid());
    }
    bytes[..bytes.len() - 1]
        .split(|byte| *byte == 0)
        .map(|entry| {
            let at = entry.iter().position(|byte| *byte == b'=').filter(|at| *at > 0).ok_or_else(invalid)?;
            Ok((OsString::from_vec(entry[..at].to_vec()), OsString::from_vec(entry[at + 1..].to_vec())))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_environment_ignores_profile_chatter_and_preserves_values() {
        let vars =
            parse(b"hello\n\0H0X_LOGIN_ENV\0PATH=/home/me/bin:/bin\0VALUE=one\ntwo=three\0\0H0X_LOGIN_END\0goodbye\n")
                .unwrap();
        assert_eq!(vars[std::ffi::OsStr::new("PATH")], "/home/me/bin:/bin");
        assert_eq!(vars[std::ffi::OsStr::new("VALUE")], "one\ntwo=three");
        assert!(parse(b"hello\n").is_err());
        assert!(parse(b"\0H0X_LOGIN_ENV\0PATH=/bin").is_err());
    }

    fn framed(body: &[u8]) -> Vec<u8> {
        [MARKER, body, END].concat()
    }

    #[test]
    fn an_empty_or_garbled_login_environment_is_refused_not_half_applied() {
        assert!(parse(b"").is_err(), "no output at all");
        assert!(parse(&framed(b"")).is_err(), "an empty environment: keep the inherited one");
        for garbage in [
            &b"PATH=/bin"[..],    // no final NUL
            b"\0",                // an empty entry
            b"PATH=/bin\0\0",     // an empty entry after a good one
            b"NOEQUALS\0",        // a word, not a setting
            b"=value\0",          // no name
            b"PATH=/bin\0junk\0", // garbage after a good one
        ] {
            assert!(parse(&framed(garbage)).is_err(), "{:?}", String::from_utf8_lossy(garbage));
        }
        // The end marker before the start marker does not count.
        assert!(parse(&[END, MARKER, b"A=1\0"].concat()).is_err());
        // Profile chatter that looks like a marker but is not one.
        assert!(parse(b"H0X_LOGIN_ENV\0A=1\0\0H0X_LOGIN_END\0").is_err());
    }

    #[test]
    fn odd_but_valid_settings_are_kept_byte_for_byte() {
        let vars = parse(&framed(b"EMPTY=\0EQ==a=b\0BIN=\xff\xfe\x01\0NL=a\nb\0")).unwrap();
        let get = |key: &str| vars[std::ffi::OsStr::new(key)].as_bytes().to_vec();
        use std::os::unix::ffi::OsStrExt;
        assert_eq!(get("EMPTY"), b"");
        assert_eq!(get("EQ"), b"=a=b");
        assert_eq!(get("BIN"), b"\xff\xfe\x01");
        assert_eq!(get("NL"), b"a\nb");
        // A repeated name: the last one wins, as a shell would export it.
        let vars = parse(&framed(b"A=1\0A=2\0")).unwrap();
        assert_eq!(vars.len(), 1);
        assert_eq!(vars[std::ffi::OsStr::new("A")], "2");
        // Only the first framed block counts; a second one is chatter.
        let vars = parse(&[framed(b"A=1\0"), framed(b"B=2\0")].concat()).unwrap();
        assert!(vars.contains_key(std::ffi::OsStr::new("A")) && !vars.contains_key(std::ffi::OsStr::new("B")));
    }

    #[test]
    fn a_huge_environment_is_parsed_whole() {
        let mut body = Vec::new();
        for i in 0..20_000 {
            body.extend_from_slice(format!("VAR_{i}={}\0", "x".repeat(60)).as_bytes());
        }
        assert!(body.len() > 1 << 20, "bigger than the capture budget, which capture() enforces, not parse()");
        let vars = parse(&framed(&body)).unwrap();
        assert_eq!(vars.len(), 20_000);
        assert_eq!(vars[std::ffi::OsStr::new("VAR_19999")], "x".repeat(60).as_str());
    }
}
