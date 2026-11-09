#[test]
fn a_restart_restores_settings_without_reusing_paths_or_lifecycle_marks() {
    let pairs =
        |pairs: &[(&str, &str)]| pairs.iter().map(|(key, value)| ((*key).into(), (*value).into())).collect::<Vec<_>>();
    let saved = pairs(&[
        ("PATH", "saved-path"),
        ("H0X_VALUE", "saved"),
        ("HOME", "old-home"),
        ("H0X_CORE_SESSION", "old-session"),
        ("H0X_SCOPE", "old-sweep"),
        ("H0X_RUNTIME", "old-runtime"),
        ("H0X_CORE_LAUNCHD_DIRECT", "old-bypass"),
        ("INVOCATION_ID", "old-service"),
        ("H0X_CORE_SOCKET", "old-socket"),
    ]);
    let current = pairs(&[
        ("PATH", "caller-path"),
        ("H0X_CALLER_ONLY", "must-not-leak"),
        ("HOME", "new-home"),
        ("H0X_CORE_SESSION", "new-session"),
        ("H0X_SCOPE", "caller-sweep"),
        ("H0X_CORE_LAUNCHD_DETACH", "caller-bypass"),
        ("INVOCATION_ID", "new-service"),
        ("H0X_CORE_SOCKET", "new-socket"),
    ]);
    let env = super::restored_environment(saved, current);
    for (key, value) in [
        ("PATH", "saved-path"),
        ("H0X_VALUE", "saved"),
        ("HOME", "new-home"),
        ("H0X_CORE_SESSION", "new-session"),
        ("H0X_CORE_SOCKET", "new-socket"),
        ("H0X_RUNTIME", "1"),
    ] {
        assert_eq!(env.get(std::ffi::OsStr::new(key)), Some(&std::ffi::OsString::from(value)));
    }
    for key in ["H0X_CALLER_ONLY", "H0X_SCOPE", "H0X_CORE_LAUNCHD_DIRECT", "H0X_CORE_LAUNCHD_DETACH"] {
        assert!(!env.contains_key(std::ffi::OsStr::new(key)), "{key}");
    }
    #[cfg(target_os = "linux")]
    assert_eq!(env.get(std::ffi::OsStr::new("INVOCATION_ID")), Some(&std::ffi::OsString::from("new-service")));
}

#[cfg(windows)]
#[test]
fn windows_path_keys_are_matched_without_case() {
    let env = super::restored_environment(
        [("home".into(), "old".into()), ("h0x_scope".into(), "old".into()), ("Path".into(), "saved".into())],
        [("HOME".into(), "new".into()), ("Path".into(), "caller".into())],
    );
    assert!(!env.contains_key(std::ffi::OsStr::new("home")));
    assert!(!env.contains_key(std::ffi::OsStr::new("h0x_scope")));
    assert_eq!(env.get(std::ffi::OsStr::new("HOME")), Some(&std::ffi::OsString::from("new")));
    assert_eq!(env.get(std::ffi::OsStr::new("Path")), Some(&std::ffi::OsString::from("saved")));
}
