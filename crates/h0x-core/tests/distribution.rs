//! Releases: the install script, `h0x-core update` from the release list, and
//! h0x-core on a machine of another OS or CPU, all against a fake releases
//! site (python3's http.server) built from this very binary.

#![cfg(unix)] // Windows: tests/windows.rs

mod common;

use common::*;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");
/// This build's version: what scripts/manifest writes, what h0x-core says it is.
const THIS: &str = env!("CARGO_PKG_VERSION");
/// A release newer than this build (it is this build inside, dressed up).
const NEWER: &str = "9.9.9";

/// A releases site with this build as `version`, for `targets`.
struct Releases {
    root: PathBuf,
    url: String,
    server: Child,
}

impl Releases {
    fn new(s: &Session, version: &str, targets: &[&str]) -> Releases {
        let root = s.root.join("releases");
        std::fs::create_dir_all(&root).unwrap();
        let ready = root.join("server-port");
        let server = Command::new("python3")
            .arg(format!("{ROOT}/tests/releases_server.py"))
            .arg(&ready)
            .current_dir(&root)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();
        let mut r = Releases { root, url: String::new(), server };
        let mut port = None;
        wait_until("the releases site", || {
            assert!(r.server.try_wait().unwrap().is_none(), "the releases site exited before becoming ready");
            port = std::fs::read_to_string(&ready).ok().and_then(|text| text.parse::<u16>().ok());
            port.is_some()
        });
        r.url = format!("http://127.0.0.1:{}", port.unwrap());
        let dir = r.root.join("download").join(format!("v{version}"));
        std::fs::create_dir_all(&dir).unwrap();
        let bin_dir = Path::new(BIN).parent().unwrap();
        for t in targets {
            let tar = dir.join(format!("h0x-core-{t}.tar.gz"));
            assert!(
                Command::new("tar")
                    .arg("-czf")
                    .arg(&tar)
                    .arg("-C")
                    .arg(bin_dir)
                    .arg("h0x-core")
                    .status()
                    .unwrap()
                    .success()
            );
        }
        let out = Command::new(format!("{ROOT}/../../scripts/manifest"))
            .arg(&dir)
            .env("H0X_CORE_RELEASES_URL", &r.url)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        // Dressed up as `version`: its field and its links.
        let manifest = String::from_utf8(out.stdout)
            .unwrap()
            .replace(&format!("\"version\": \"{THIS}\""), &format!("\"version\": \"{version}\""))
            .replace(&format!("/download/v{THIS}/"), &format!("/download/v{version}/"));
        std::fs::write(dir.join("manifest.json"), &manifest).unwrap();
        std::fs::create_dir_all(r.root.join("latest/download")).unwrap();
        std::fs::write(r.root.join("latest/download/latest.json"), &manifest).unwrap();
        r
    }
}

impl Drop for Releases {
    fn drop(&mut self) {
        let _ = self.server.kill();
        let _ = self.server.wait();
    }
}

fn here() -> String {
    let arch = if std::env::consts::ARCH == "aarch64" { "aarch64" } else { "x86_64" };
    format!("{}-{arch}", std::env::consts::OS)
}

#[test]
fn the_install_script_installs_and_then_updates_without_stopping_anything() {
    let mut s = Session::new();
    let r = Releases::new(&s, THIS, &[&here()]);
    let bin = s.root.join("bin");
    let install = |s: &Session| {
        let mut cmd = s.program("sh");
        cmd.arg(format!("{ROOT}/../../distribution/install.sh")).env("PATH", "/usr/bin:/bin");
        let out = cmd.env("H0X_CORE_RELEASES_URL", &r.url).env("H0X_CORE_INSTALL_DIR", &bin).output().unwrap();
        assert!(
            out.status.success(),
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    assert!(install(&s).contains("installed"));
    assert!(bin.join("h0x-core").is_file());
    assert!(std::fs::read_dir(&bin).unwrap().count() == 1, "only h0x-core is installed: no alias beside it");

    s.bin = bin.join("h0x-core");
    let t = run(&s, &["sh", "-c", "while read l; do echo got-$l; done"]);
    let pid = s.json(&["server", "status"])["pid"].clone();
    assert!(install(&s).contains("updated"), "again: an update");
    assert_ne!(s.json(&["server", "status"])["pid"], pid, "the planned restart");
    s.json(&["send", &t, "still"]);
    wait_until("the terminal kept running", || text(&s, &t).contains("got-still"));
    drop(r);
}

/// The URL the install script downloads h0x-core from, for a list giving `url`,
/// with curl stubbed to record it (the download itself fails).
fn install_script_downloads(s: &Session, url: &str, releases: &str, installed: bool) -> String {
    let dir = s.root.join(format!(
        "marker-{}",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ));
    let (stub, bin) = (dir.join("stub"), dir.join("bin"));
    std::fs::create_dir_all(&stub).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    if installed {
        std::fs::write(bin.join("h0x-core"), "#!/bin/sh\n").unwrap();
        Command::new("chmod").arg("755").arg(bin.join("h0x-core")).status().unwrap();
    }
    let list = dir.join("latest.json");
    std::fs::write(
        &list,
        format!(r#"{{"version":"1.0.0","generation":1,"targets":{{"{}":{{"url":"{url}","sha256":"x"}}}}}}"#, here()),
    )
    .unwrap();
    let log = dir.join("urls");
    std::fs::write(
        stub.join("curl"),
        format!(
            "#!/bin/sh\nout=; url=; prev=\nfor a; do\n  if [ \"$prev\" = -o ]; then out=$a; else case $a in -*) ;; *) url=$a ;; esac; fi\n  prev=$a\ndone\necho \"$url\" >> {log}\ncase $url in */latest.json) cp {list} \"$out\"; exit 0 ;; esac\nexit 22\n",
            log = log.display(),
            list = list.display()
        ),
    )
    .unwrap();
    Command::new("chmod").arg("755").arg(stub.join("curl")).status().unwrap();
    let out = Command::new("sh")
        .arg(format!("{ROOT}/../../distribution/install.sh"))
        .env("PATH", format!("{}:/usr/bin:/bin", stub.display()))
        .env("H0X_CORE_RELEASES_URL", releases)
        .env_remove("H0X_CORE_VERSION")
        .env("H0X_CORE_INSTALL_DIR", &bin)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("download failed"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::read_to_string(log).unwrap().lines().last().unwrap().to_owned()
}

/// The install script fetches the list's URL exactly as given, installing or
/// updating: a release file, a mirror's query (a signed URL) or fragment,
/// or another host.
#[test]
fn the_install_script_downloads_exactly_what_the_list_gives() {
    let s = Session::new();
    let r = "https://releases.example";
    let tarball = format!("{r}/download/v1.0.0/h0x-core-{}.tar.gz", here());
    assert_eq!(install_script_downloads(&s, &tarball, r, false), tarball);
    assert_eq!(install_script_downloads(&s, &tarball, r, true), tarball);
    for url in [
        format!("{tarball}?sig=a%2Fb&exp=1"),
        format!("{tarball}#x"),
        "https://mirror.example/download/v1.0.0/h0x-core.tar.gz".to_owned(),
        "https://releases.example.evil/download/v1.0.0/h0x-core.tar.gz".to_owned(),
    ] {
        assert_eq!(install_script_downloads(&s, &url, r, false), url);
    }
}

#[test]
fn update_downloads_checks_and_swaps() {
    let mut s = Session::new();
    let r = Releases::new(&s, NEWER, &[&here()]);
    let dir = s.root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(BIN, dir.join("h0x-core")).unwrap();
    s.bin = dir.join("h0x-core");
    s.env.push(("H0X_CORE_RELEASES_URL".into(), r.url.clone()));
    let pid = s.json(&["server", "status"])["pid"].clone();

    let up = s.json(&["update"]);
    assert_eq!((up["restarted"].as_bool(), up["release"].as_str()), (Some(true), Some(NEWER)), "{up}");
    assert_ne!(s.json(&["server", "status"])["pid"], pid);

    let tar = r.root.join(format!("download/v{NEWER}/h0x-core-{}.tar.gz", here()));
    std::fs::write(&tar, b"not what the list says").unwrap();
    let (_, e) = s.err(&["--json", "update"]);
    assert_eq!(e["error"]["kind"], "refused", "a damaged download changes nothing: {e}");
    drop(r);
}

#[test]
fn update_to_a_version_takes_at_least_that_one() {
    let mut s = Session::new();
    let r = Releases::new(&s, NEWER, &[&here()]);
    let dir = s.root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(BIN, dir.join("h0x-core")).unwrap();
    s.bin = dir.join("h0x-core");
    s.env.push(("H0X_CORE_RELEASES_URL".into(), r.url.clone()));
    s.json(&["server", "status"]);

    let up = s.json(&["update", "--version", NEWER]);
    assert_eq!((up["restarted"].as_bool(), up["release"].as_str()), (Some(true), Some(NEWER)), "{up}");
    // Already that or newer: nothing changes, nothing is downloaded.
    let same = s.json(&["update", "--version", "0.0.1"]);
    assert_eq!((same["updated"].as_bool(), same["release"].as_str()), (Some(false), Some("0.0.1")), "{same}");
    let (_, e) = s.err(&["--json", "update", "--version", "9.9.10"]);
    assert_eq!(e["error"]["kind"], "unavailable", "no such release: {e}");
    let (_, e) = s.err(&["--json", "update", "--version", "../x"]);
    assert_eq!(e["error"]["kind"], "usage", "{e}");
    drop(r);
}

#[test]
fn update_never_takes_a_release_of_another_generation() {
    let mut s = Session::new();
    let r = Releases::new(&s, NEWER, &[&here()]);
    let latest = r.root.join("latest/download/latest.json");
    let breaking = std::fs::read_to_string(&latest).unwrap().replace("\"generation\": 1", "\"generation\": 2");
    std::fs::write(&latest, breaking).unwrap();
    let dir = s.root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(BIN, dir.join("h0x-core")).unwrap(); // never the build under test
    s.bin = dir.join("h0x-core");
    s.env.push(("H0X_CORE_RELEASES_URL".into(), r.url.clone()));
    let (_, e) = s.err(&["--json", "update"]);
    assert_eq!(e["error"]["kind"], "refused", "apps built for this generation must keep working: {e}");
    assert!(e["error"]["message"].as_str().unwrap().contains("generation"), "{e}");
    drop(r);
}

#[test]
fn a_machine_of_another_cpu_gets_the_released_build_for_it() {
    let mut odd = Session::new();
    odd.bin = odd.root.join("bin/h0x-core"); // no h0x-core there yet
    std::fs::create_dir_all(odd.root.join("fakebin")).unwrap();
    let other = if std::env::consts::ARCH == "aarch64" { "x86_64" } else { "aarch64" };
    let os = if std::env::consts::OS == "macos" { "Darwin" } else { "Linux" };
    let uname = odd.root.join("fakebin/uname");
    std::fs::write(&uname, format!("#!/bin/sh\necho '{os} {other}'\n")).unwrap();
    std::fs::set_permissions(&uname, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    let mut s = Session::new();
    // The "other CPU" build is this binary, so it runs on the fake machine.
    let target = format!("{}-{other}", std::env::consts::OS);
    let r = Releases::new(&s, THIS, &[&target]);
    s.env.push(fake_ssh(&s, &[("odd", &odd)]));
    s.env.push(("H0X_CORE_RELEASES_URL".into(), r.url.clone()));
    let t = run(&s, &["sh", "-c", &format!("{BIN} machine add odd; echo added-$?; sleep 100")]);
    wait_until("the offer", || text(&s, &t).contains("[Y/n]"));
    s.json(&["send", &t, "y"]);
    wait_until("added", || text(&s, &t).contains("added-0"));
    assert!(odd.root.join(".local/bin/h0x-core").is_file());
    odd.bin = odd.root.join(".local/bin/h0x-core");
    drop(r);
}

/// A release archive of this build named as the release names it, in a fresh folder.
fn archive_of_this_build(s: &Session, tag: &str) -> PathBuf {
    let dir = s.root.join(format!("archive-{tag}"));
    std::fs::create_dir_all(&dir).unwrap();
    let tar = dir.join(format!("h0x-core-{}.tar.gz", here()));
    let bin_dir = Path::new(BIN).parent().unwrap();
    assert!(
        Command::new("tar").arg("-czf").arg(&tar).arg("-C").arg(bin_dir).arg("h0x-core").status().unwrap().success()
    );
    tar
}

/// `update --from` takes a release archive only with the release key's
/// signature for that very file; every refusal leaves the installed h0x-core
/// byte for byte as it was. (A good signature needs the release secret key:
/// the unit tests in releases.rs cover it with a throwaway key.)
#[test]
fn update_from_an_archive_without_a_good_signature_swaps_nothing() {
    let mut s = Session::new();
    let dir = s.root.join("bin");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::copy(BIN, dir.join("h0x-core")).unwrap();
    s.bin = dir.join("h0x-core");
    let before = std::fs::read(&s.bin).unwrap();
    let tar = archive_of_this_build(&s, "a");
    let sig = tar.with_file_name(format!("{}.minisig", tar.file_name().unwrap().to_str().unwrap()));
    let stray = s.root.join("elsewhere.minisig");
    let tar_arg = tar.to_str().unwrap();
    let sig_arg = sig.to_str().unwrap();
    let junk = "untrusted comment: x\nnot a signature\ntrusted comment: file:x\nnot a signature\n";
    let refusals = |args: &[&str], why: &str| {
        let mut all = vec!["--json", "update"];
        all.extend_from_slice(args);
        let (_, e) = s.err(&all);
        assert_eq!(e["error"]["kind"], "refused", "{args:?}: {e}");
        let message = e["error"]["message"].as_str().unwrap();
        assert!(message.contains(why), "{args:?}: {message}");
        assert_eq!(std::fs::read(dir.join("h0x-core")).unwrap(), before, "nothing was swapped: {args:?}");
        assert!(tar.exists(), "the person's archive stays");
    };
    // No signature at all, then garbage, then a signature that is not the release key's.
    refusals(&["--from", tar_arg], "it has no signature");
    refusals(&["--from", tar_arg, "--check"], "it has no signature");
    std::fs::write(&sig, junk).unwrap();
    refusals(&["--from", tar_arg], "its signature is unreadable");
    std::fs::write(&stray, junk).unwrap();
    refusals(&["--from", tar_arg, "--signature", stray.to_str().unwrap()], "its signature is unreadable");
    refusals(&["--from", tar_arg, "--signature", s.root.join("none.minisig").to_str().unwrap()], "it has no signature");
    std::fs::remove_file(&sig).unwrap();
    // A tampered archive: still no valid signature.
    std::fs::write(&tar, b"not what was signed").unwrap();
    std::fs::write(&sig, junk).unwrap();
    refusals(&["--from", tar_arg], "not signed by the h0x-core release key");
    // A signature file given for a bare binary is no way around it.
    refusals(&["--from", BIN, "--signature", sig_arg], "is not a signed release archive");
}

/// install.sh without a test site asks for the asset's `.minisig` and lets the
/// downloaded h0x-core check it before anything lands in the install folder.
/// curl is stubbed to serve a manifest and this build's archive.
#[test]
fn the_install_script_refuses_a_download_without_the_release_signature() {
    let s = Session::new();
    let tar = archive_of_this_build(&s, "i");
    let name = tar.file_name().unwrap().to_str().unwrap().to_owned();
    let sum = Command::new("sh")
        .arg("-c")
        .arg(format!("sha256sum {0} 2>/dev/null || shasum -a 256 {0}", tar.display()))
        .output()
        .unwrap();
    let sum = String::from_utf8(sum.stdout).unwrap().split_whitespace().next().unwrap().to_owned();
    let url = format!("https://github.com/vjk7989/h0x-ADE-core/releases/download/v1.0.0/{name}");
    let manifest = tar.with_file_name("latest.json");
    std::fs::write(
        &manifest,
        format!(
            r#"{{"version":"1.0.0","generation":1,"targets":{{"{}":{{"url":"{url}","sha256":"{sum}"}}}}}}"#,
            here()
        ),
    )
    .unwrap();
    let junk = tar.with_file_name("junk.minisig");
    std::fs::write(&junk, "untrusted comment: x\nnot a signature\ntrusted comment: file:x\nnot a signature\n").unwrap();
    let stub = s.root.join("stub");
    std::fs::create_dir_all(&stub).unwrap();
    let script = |serve_sig: bool| {
        format!(
            "#!/bin/sh\nout=; url=; prev=\nfor a; do\n  if [ \"$prev\" = -o ]; then out=$a; else case $a in -*) ;; *) url=$a ;; esac; fi\n  prev=$a\ndone\ncase $url in\n  */latest.json) cp {manifest} \"$out\" ;;\n  */{name}.minisig) {sig} ;;\n  */{name}) cp {tar} \"$out\" ;;\n  *) exit 22 ;;\nesac\n",
            manifest = manifest.display(),
            tar = tar.display(),
            sig = if serve_sig { format!("cp {} \"$out\"", junk.display()) } else { "exit 22".to_owned() },
        )
    };
    let install = |serve_sig: bool| {
        std::fs::write(stub.join("curl"), script(serve_sig)).unwrap();
        Command::new("chmod").arg("755").arg(stub.join("curl")).status().unwrap();
        let bin = s.root.join(format!("installed-{serve_sig}"));
        let out = Command::new("sh")
            .arg(format!("{ROOT}/../../distribution/install.sh"))
            .env("PATH", format!("{}:/usr/bin:/bin", stub.display()))
            .env_remove("H0X_CORE_RELEASES_URL")
            .env_remove("H0X_CORE_VERSION")
            .env("H0X_CORE_INSTALL_DIR", &bin)
            .output()
            .unwrap();
        assert!(!out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        assert!(!bin.join("h0x-core").exists(), "nothing is installed");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    assert!(install(false).contains(&format!("no signature for {name}")));
    assert!(install(true).contains(&format!("{name} is not signed by the h0x-core release key")));
}

/// Upgrading an h0x-core older than release signing: it takes only a bare
/// binary for `update --from`, so install.sh hands it the binary the new
/// h0x-core already verified. A bad signature swaps nothing. The new h0x-core
/// is a script standing in (`--check` passes or fails as told); the real
/// check needs the release secret key.
#[test]
fn the_install_script_upgrades_a_core_that_takes_only_a_bare_binary_after_verifying() {
    let s = Session::new();
    let dir = s.root.join("old-core");
    std::fs::create_dir_all(&dir).unwrap();
    let name = format!("h0x-core-{}.tar.gz", here());
    let make_new = |check_exit: i32| {
        let new = dir.join("h0x-core");
        std::fs::write(
            &new,
            format!(
                "#!/bin/sh\ncase \"$*\" in\n  --version) echo 'h0x-core 9.9.9'; exit 0 ;;\n  *--check*) exit {check_exit} ;;\nesac\nexit 0 # new core {check_exit}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&new, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let tar = dir.join(&name);
        assert!(
            Command::new("tar").arg("-czf").arg(&tar).arg("-C").arg(&dir).arg("h0x-core").status().unwrap().success()
        );
        (std::fs::read(&new).unwrap(), tar)
    };
    let sum = |tar: &Path| {
        let out = Command::new("sh")
            .arg("-c")
            .arg(format!("sha256sum {0} 2>/dev/null || shasum -a 256 {0}", tar.display()))
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap().split_whitespace().next().unwrap().to_owned()
    };
    let stub = s.root.join("stub");
    std::fs::create_dir_all(&stub).unwrap();
    let old_core = "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'h0x-core 0.1.8' ;;\n  update) case \"$3\" in *.tar.gz | *.zip) echo 'not a working h0x-core' >&2; exit 1 ;; esac; cp \"$3\" \"$0\" ;;\nesac\n";
    for (check_exit, upgraded) in [(0, true), (1, false)] {
        let (new_bytes, tar) = make_new(check_exit);
        let url = format!("https://github.com/vjk7989/h0x-ADE-core/releases/download/v1.0.0/{name}");
        let manifest = dir.join("latest.json");
        std::fs::write(
            &manifest,
            format!(
                r#"{{"version":"1.0.0","generation":1,"targets":{{"{}":{{"url":"{url}","sha256":"{}"}}}}}}"#,
                here(),
                sum(&tar)
            ),
        )
        .unwrap();
        std::fs::write(
            stub.join("curl"),
            format!(
                "#!/bin/sh\nout=; url=; prev=\nfor a; do\n  if [ \"$prev\" = -o ]; then out=$a; else case $a in -*) ;; *) url=$a ;; esac; fi\n  prev=$a\ndone\ncase $url in\n  */latest.json) cp {manifest} \"$out\" ;;\n  */{name}.minisig) echo sig > \"$out\" ;;\n  */{name}) cp {tar} \"$out\" ;;\n  *) exit 22 ;;\nesac\n",
                manifest = manifest.display(),
                tar = tar.display()
            ),
        )
        .unwrap();
        Command::new("chmod").arg("755").arg(stub.join("curl")).status().unwrap();
        let bin = s.root.join(format!("installed-{check_exit}"));
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("h0x-core"), old_core).unwrap();
        Command::new("chmod").arg("755").arg(bin.join("h0x-core")).status().unwrap();
        let out = Command::new("sh")
            .arg(format!("{ROOT}/../../distribution/install.sh"))
            .env("PATH", format!("{}:/usr/bin:/bin", stub.display()))
            .env_remove("H0X_CORE_RELEASES_URL")
            .env_remove("H0X_CORE_VERSION")
            .env("H0X_CORE_INSTALL_DIR", &bin)
            .output()
            .unwrap();
        let installed = std::fs::read(bin.join("h0x-core")).unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        if upgraded {
            assert!(out.status.success(), "{err}");
            assert_eq!(installed, new_bytes, "the verified new binary is in place");
        } else {
            assert!(!out.status.success());
            assert!(err.contains("is not signed by the h0x-core release key"), "{err}");
            assert_eq!(installed, old_core.as_bytes(), "a bad signature swaps nothing");
        }
    }
}

/// A stubbed signed release for install.sh: `new_core` (a script) packed as this
/// target's archive, listed at its release URL plus `query`, and a curl in the
/// returned folder that serves the list, the archive and a `.minisig`, logging
/// every URL into `<root>/curl.log`. Also the archive's name and the script's bytes.
fn stubbed_release(s: &Session, query: &str, new_core: &str) -> (PathBuf, String, Vec<u8>) {
    let dir = s.root.join("release");
    std::fs::create_dir_all(&dir).unwrap();
    let new = dir.join("h0x-core");
    std::fs::write(&new, new_core).unwrap();
    std::fs::set_permissions(&new, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let name = format!("h0x-core-{}.tar.gz", here());
    let tar = dir.join(&name);
    assert!(Command::new("tar").arg("-czf").arg(&tar).arg("-C").arg(&dir).arg("h0x-core").status().unwrap().success());
    let sum = Command::new("sh")
        .arg("-c")
        .arg(format!("sha256sum {0} 2>/dev/null || shasum -a 256 {0}", tar.display()))
        .output()
        .unwrap();
    let sum = String::from_utf8(sum.stdout).unwrap().split_whitespace().next().unwrap().to_owned();
    let url = format!("https://github.com/vjk7989/h0x-ADE-core/releases/download/v1.0.0/{name}{query}");
    let manifest = dir.join("latest.json");
    std::fs::write(
        &manifest,
        format!(
            r#"{{"version":"1.0.0","generation":1,"targets":{{"{}":{{"url":"{url}","sha256":"{sum}"}}}}}}"#,
            here()
        ),
    )
    .unwrap();
    let stub = s.root.join("stub");
    std::fs::create_dir_all(&stub).unwrap();
    std::fs::write(
        stub.join("curl"),
        format!(
            "#!/bin/sh\nout=; url=; prev=\nfor a; do\n  if [ \"$prev\" = -o ]; then out=$a; else case $a in -*) ;; *) url=$a ;; esac; fi\n  prev=$a\ndone\nprintf '%s\\n' \"$url\" >> '{log}'\ncase $url in\n  */latest.json) cp '{manifest}' \"$out\" ;;\n  *.minisig) echo sig > \"$out\" ;;\n  *) cp '{tar}' \"$out\" ;;\nesac\n",
            log = s.root.join("curl.log").display(),
            manifest = manifest.display(),
            tar = tar.display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(stub.join("curl"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    (stub, name, std::fs::read(&new).unwrap())
}

/// install.sh against the default (signed) site, curl from `stub`, into `bin`.
fn install_signed(stub: &Path, bin: &Path) -> std::process::Output {
    Command::new("sh")
        .arg(format!("{ROOT}/../../distribution/install.sh"))
        .env("PATH", format!("{}:/usr/bin:/bin", stub.display()))
        .env_remove("H0X_CORE_RELEASES_URL")
        .env_remove("H0X_CORE_VERSION")
        .env("H0X_CORE_INSTALL_DIR", bin)
        .output()
        .unwrap()
}

/// A new h0x-core stand-in whose `--check` passes and logs its arguments.
fn checking_core(s: &Session) -> String {
    format!(
        "#!/bin/sh\ncase \"$*\" in\n  --version) echo 'h0x-core 9.9.9' ;;\n  *--check*) printf '%s\\n' \"$*\" >> '{}' ;;\nesac\nexit 0\n",
        s.root.join("check.log").display()
    )
}

/// When the installed h0x-core takes neither the archive nor the bare binary,
/// both refusals reach the person and the installed h0x-core stays as it was.
#[test]
fn the_install_script_shows_both_refusals_when_no_update_takes_and_keeps_the_old_core() {
    let s = Session::new();
    let (stub, name, _) = stubbed_release(&s, "", &checking_core(&s));
    let bin = s.root.join("installed");
    std::fs::create_dir_all(&bin).unwrap();
    let old_core = "#!/bin/sh\ncase \"$1\" in\n  --version) echo 'h0x-core 0.1.8' ;;\n  update) echo \"cannot take $3\" >&2; exit 1 ;;\nesac\n";
    std::fs::write(bin.join("h0x-core"), old_core).unwrap();
    std::fs::set_permissions(bin.join("h0x-core"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let out = install_signed(&stub, &bin);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{err}");
    let refusals: Vec<&str> = err.lines().filter(|l| l.starts_with("cannot take ")).collect();
    assert_eq!(refusals.len(), 2, "{err}");
    assert!(refusals[0].ends_with(&format!("/{name}")), "the archive first: {err}");
    assert!(refusals[1].ends_with("/h0x-core"), "then the verified binary: {err}");
    assert!(err.contains(&format!("updating {}/h0x-core failed", bin.display())), "{err}");
    assert_eq!(std::fs::read(bin.join("h0x-core")).unwrap(), old_core.as_bytes(), "nothing swapped");
    assert!(!bin.join("h0x-core.new").exists());
}

/// A fresh install (no h0x-core there yet) into a folder with spaces, from a
/// list whose URL carries a query: the `.minisig` is asked for without the
/// query, and `--check` gets the archive under its release name.
#[test]
fn the_install_script_installs_fresh_from_a_url_with_a_query_into_a_folder_with_spaces() {
    let s = Session::new();
    let (stub, name, new_bytes) = stubbed_release(&s, "?sig=abc&exp=1#top", &checking_core(&s));
    let bin = s.root.join("my bin ü");
    let out = install_signed(&stub, &bin);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read(bin.join("h0x-core")).unwrap(), new_bytes, "the checked binary is installed");
    let base = format!("https://github.com/vjk7989/h0x-ADE-core/releases/download/v1.0.0/{name}");
    let asked = std::fs::read_to_string(s.root.join("curl.log")).unwrap();
    assert!(asked.lines().any(|l| l == format!("{base}?sig=abc&exp=1#top")), "the archive as listed: {asked}");
    assert!(asked.lines().any(|l| l == format!("{base}.minisig")), "the signature without the query: {asked}");
    let checked = std::fs::read_to_string(s.root.join("check.log")).unwrap();
    let line = checked.lines().next().unwrap_or_default();
    assert!(line.starts_with("update --from ") && line.ends_with(&format!("/{name} --check")), "{checked}");
}

/// The signature is bound to the asset's file name, so the archive must be
/// saved under it even when the list's URL has a `/` in its query.
#[test]
fn the_install_script_names_the_archive_after_the_path_even_when_the_query_has_a_slash() {
    let s = Session::new();
    let (stub, name, _) = stubbed_release(&s, "?token=a/b", &checking_core(&s));
    let bin = s.root.join("installed");
    let out = install_signed(&stub, &bin);
    let checked = std::fs::read_to_string(s.root.join("check.log")).unwrap_or_default();
    assert!(
        checked.lines().any(|l| l.ends_with(&format!("/{name} --check"))),
        "--check got: {checked}; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An h0x-core already installed (0.1.9 or newer) is the one that checks the
/// signature: a downloaded h0x-core that would lie ("signed") or refuse never
/// gets to say, and it is not even asked.
#[test]
fn the_install_script_lets_the_installed_core_verify_not_the_downloaded_one() {
    for installed_accepts in [false, true] {
        let s = Session::new();
        let asked = s.root.join("asked.log");
        let downloaded_exit = i32::from(installed_accepts);
        let downloaded = format!(
            "#!/bin/sh\ncase \"$*\" in\n  --version) echo 'h0x-core 9.9.9' ;;\n  *--check*) echo downloaded >> '{}'; exit {downloaded_exit} ;;\nesac\nexit 0\n",
            asked.display()
        );
        let (stub, _name, new_bytes) = stubbed_release(&s, "", &downloaded);
        let bin = s.root.join("installed");
        std::fs::create_dir_all(&bin).unwrap();
        let installed = format!(
            "#!/bin/sh\ncase \"$*\" in\n  --version) echo 'h0x-core 0.1.9' ;;\n  *--check*) echo installed >> '{}'; exit {} ;;\n  update*) case \"$3\" in *.tar.gz) exit 1 ;; esac; cp \"$3\" \"$0\" ;;\nesac\n",
            asked.display(),
            i32::from(!installed_accepts)
        );
        std::fs::write(bin.join("h0x-core"), &installed).unwrap();
        std::fs::set_permissions(bin.join("h0x-core"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let out = install_signed(&stub, &bin);
        let err = String::from_utf8_lossy(&out.stderr);
        let log = std::fs::read_to_string(&asked).unwrap_or_default();
        assert_eq!(log.trim(), "installed", "only the installed core is asked: {log:?} {err}");
        let now = std::fs::read(bin.join("h0x-core")).unwrap();
        if installed_accepts {
            assert!(out.status.success(), "{err}");
            assert_eq!(now, new_bytes, "the verified download is swapped in");
        } else {
            assert!(!out.status.success());
            assert!(err.contains("is not signed by the h0x-core release key"), "{err}");
            assert_eq!(now, installed.as_bytes(), "a refusal swaps nothing");
        }
    }
}

/// A first install has no h0x-core to ask: the downloaded one checks (trust on
/// first use), and the script says so.
#[test]
fn a_first_install_says_the_downloaded_core_checked_its_own_signature() {
    let s = Session::new();
    let (stub, _name, _) = stubbed_release(&s, "", &checking_core(&s));
    let out = install_signed(&stub, &s.root.join("fresh"));
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.contains("trusted on first use"), "{said}");
}

/// Which h0x-core checks the download depends on the installed one's `--version`,
/// compared as numbers (0.1.10 is newer than 0.1.9): 0.1.9 or newer checks it
/// itself; older, unreadable or silent ones leave it to the downloaded core,
/// which says so. Installed into a folder with spaces.
#[test]
fn the_install_script_reads_the_installed_version_as_numbers_to_pick_the_verifier() {
    let cases = [
        ("echo 'h0x-core 0.1.10'", "installed"),
        ("echo 'h0x-core 0.2.0'", "installed"),
        ("echo 'h0x-core 1.0.0'", "installed"),
        ("echo 'h0x-core 0.1.10-rc.1'", "installed"),
        ("echo 'h0x-core 0.1.9 (abc123)'", "installed"),
        ("echo 'h0x-core 0.1.8'", "downloaded"),
        ("echo 'h0x-core 0.0.99'", "downloaded"),
        ("echo 'h0x-core 0.1'", "downloaded"),
        ("echo 'hc 0.1.9'", "downloaded"),
        ("echo 'h0x-core'", "downloaded"),
        ("exit 1", "downloaded"),
    ];
    for (says, verifier) in cases {
        let s = Session::new();
        let asked = s.root.join("asked.log");
        let downloaded = format!(
            "#!/bin/sh\ncase \"$*\" in\n  --version) echo 'h0x-core 9.9.9' ;;\n  *--check*) echo downloaded >> '{}' ;;\nesac\nexit 0\n",
            asked.display()
        );
        let (stub, _name, new_bytes) = stubbed_release(&s, "", &downloaded);
        let bin = s.root.join("installed core ü");
        std::fs::create_dir_all(&bin).unwrap();
        let installed = format!(
            "#!/bin/sh\ncase \"$*\" in\n  --version) {says} ;;\n  *--check*) echo installed >> '{}' ;;\n  update*) case \"$3\" in *.tar.gz) exit 1 ;; esac; cp \"$3\" \"$0\" ;;\nesac\n",
            asked.display()
        );
        std::fs::write(bin.join("h0x-core"), &installed).unwrap();
        std::fs::set_permissions(bin.join("h0x-core"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let out = install_signed(&stub, &bin);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{says}: {err}");
        let log = std::fs::read_to_string(&asked).unwrap_or_default();
        assert_eq!(log.trim(), verifier, "{says}: exactly one verifier is asked: {log:?} {err}");
        let said = String::from_utf8_lossy(&out.stdout);
        assert_eq!(said.contains("trusted on first use"), verifier == "downloaded", "{says}: {said}");
        assert_eq!(std::fs::read(bin.join("h0x-core")).unwrap(), new_bytes, "{says}: swapped once verified");
    }
}

/// install.sh with the installed core's runs bounded to `secs` seconds.
fn install_signed_within(stub: &Path, bin: &Path, secs: &str) -> std::process::Output {
    Command::new("sh")
        .arg(format!("{ROOT}/../../distribution/install.sh"))
        .env("PATH", format!("{}:/usr/bin:/bin", stub.display()))
        .env_remove("H0X_CORE_RELEASES_URL")
        .env_remove("H0X_CORE_VERSION")
        .env("H0X_CORE_INSTALL_SECS", secs)
        .env("H0X_CORE_INSTALL_DIR", bin)
        .output()
        .unwrap()
}

fn put_installed(bin: &Path, script: &str) {
    std::fs::create_dir_all(bin).unwrap();
    std::fs::write(bin.join("h0x-core"), script).unwrap();
    std::fs::set_permissions(bin.join("h0x-core"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
}

/// An installed core that hangs on --version cannot say what it is: the
/// downloaded one checks instead. One that hangs on --check fails the install
/// (verification is never skipped).
#[test]
fn the_install_script_does_not_hang_on_an_installed_core_that_hangs() {
    let s = Session::new();
    let asked = s.root.join("asked.log");
    let downloaded = format!(
        "#!/bin/sh\ncase \"$*\" in\n  --version) echo 'h0x-core 9.9.9' ;;\n  *--check*) echo downloaded >> '{}' ;;\nesac\nexit 0\n",
        asked.display()
    );
    let (stub, _name, new_bytes) = stubbed_release(&s, "", &downloaded);
    // --version hangs (in a child, as a real stuck process might).
    let bin = s.root.join("hangs-version");
    put_installed(
        &bin,
        "#!/bin/sh\ncase \"$1\" in\n  --version) sleep 600 ;;\n  update*) case \"$3\" in *.tar.gz) exit 1 ;; esac; cp \"$3\" \"$0\" ;;\nesac\n",
    );
    let started = std::time::Instant::now();
    let out = install_signed_within(&stub, &bin, "1");
    assert!(started.elapsed() < std::time::Duration::from_secs(60), "bounded");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read_to_string(&asked).unwrap().trim(), "downloaded", "the downloaded core checked");
    assert_eq!(std::fs::read(bin.join("h0x-core")).unwrap(), new_bytes);
    // --check hangs: the install fails and nothing is swapped.
    let bin = s.root.join("hangs-check");
    let hangs = "#!/bin/sh\ncase \"$*\" in\n  --version) echo 'h0x-core 0.1.9' ;;\n  *--check*) sleep 600 ;;\nesac\n";
    put_installed(&bin, hangs);
    let started = std::time::Instant::now();
    let out = install_signed_within(&stub, &bin, "1");
    assert!(started.elapsed() < std::time::Duration::from_secs(60), "bounded");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("is not signed by the h0x-core release key"));
    assert_eq!(std::fs::read(bin.join("h0x-core")).unwrap(), hangs.as_bytes());
}

/// A prerelease of 0.1.9 is older than 0.1.9: it is not trusted to verify.
#[test]
fn a_prerelease_installed_core_is_older_and_the_downloaded_core_verifies() {
    for version in ["0.1.9-rc.1", "0.1.9+build", "99999999999.0.0", "0.1", "junk"] {
        let s = Session::new();
        let asked = s.root.join("asked.log");
        let downloaded = format!(
            "#!/bin/sh\ncase \"$*\" in\n  --version) echo 'h0x-core 9.9.9' ;;\n  *--check*) echo downloaded >> '{}' ;;\nesac\nexit 0\n",
            asked.display()
        );
        let (stub, _name, _) = stubbed_release(&s, "", &downloaded);
        let bin = s.root.join("installed");
        put_installed(
            &bin,
            &format!(
                "#!/bin/sh\ncase \"$*\" in\n  --version) echo 'h0x-core {version}' ;;\n  *--check*) echo installed >> '{}'; exit 1 ;;\n  update*) case \"$3\" in *.tar.gz) exit 1 ;; esac; cp \"$3\" \"$0\" ;;\nesac\n",
                asked.display()
            ),
        );
        let out = install_signed_within(&stub, &bin, "5");
        let log = std::fs::read_to_string(&asked).unwrap_or_default();
        assert_eq!(log.trim(), "downloaded", "{version}: {log:?} {}", String::from_utf8_lossy(&out.stderr));
    }
}
