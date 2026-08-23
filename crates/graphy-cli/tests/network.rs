#[cfg(not(feature = "outbound-http"))]
use std::process::Command;

#[cfg(not(feature = "outbound-http"))]
#[test]
fn allow_network_requires_compile_time_opt_in() {
    let output = Command::new(env!("CARGO_BIN_EXE_graphy"))
        .args(["serve", "/definitely/not/a/graphy/store", "--allow-network"])
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("--allow-network requires a binary built with `--features outbound-http`"),
        "{stderr}"
    );
}
