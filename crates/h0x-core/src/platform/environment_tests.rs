#[test]
fn a_saved_absence_removes_an_inherited_manager_value() {
    let line = if cfg!(windows) {
        "if defined H0X_CORE_RESTART_ABSENT (echo leaked) else (echo %H0X_CORE_RESTART_VALUE%)"
    } else {
        "if [ \"${H0X_CORE_RESTART_ABSENT+x}\" ]; then echo leaked; else echo \"$H0X_CORE_RESTART_VALUE\"; fi"
    };
    let argv = super::shell_line(line);
    let mut cmd = std::process::Command::new(&argv[0]);
    cmd.args(&argv[1..]).env("H0X_CORE_RESTART_ABSENT", "manager").env("H0X_CORE_RESTART_VALUE", "caller");
    super::replace_env(
        &mut cmd,
        [("H0X_CORE_RESTART_VALUE".into(), Some("saved".into())), ("H0X_CORE_RESTART_ABSENT".into(), None)],
    );
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), "saved");
}
