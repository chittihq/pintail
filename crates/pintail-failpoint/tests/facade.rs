#[test]
fn environment_is_effective_only_with_the_feature() {
    if std::env::var_os("PINTAIL_FAILPOINT_FACADE_CHILD").is_some() {
        pintail_failpoint::hit("unknown").unwrap();
        pintail_failpoint::hit("facade").unwrap();
        let second = pintail_failpoint::hit("facade");
        assert_eq!(second.is_err(), cfg!(feature = "failpoints"));
        pintail_failpoint::hit("facade").unwrap();
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("environment_is_effective_only_with_the_feature")
        .arg("--nocapture")
        .env("PINTAIL_FAILPOINT_FACADE_CHILD", "1")
        .env("PINTAIL_FAILPOINT", "facade@2=error")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr.matches("failpoint facade hit 2: error").count(),
        usize::from(cfg!(feature = "failpoints"))
    );
}
