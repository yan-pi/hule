use std::process::Command;

#[test]
fn version_flag_prints_package_version() {
    let output = Command::new(env!("CARGO_BIN_EXE_hule"))
        .arg("--version")
        .output()
        .expect("run hule");

    assert!(output.status.success());

    let stout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    assert_eq!(stout.trim(), format!("hule {}", env!("CARGO_PKG_VERSION")));
}
