#[test]
fn messenger_server_and_clients() {
    let output = std::process::Command::new("python3")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/verify-messenger.py"))
        .arg(env!("CARGO_BIN_EXE_ccs"))
        .output()
        .expect("python3 is required for the messenger integration check");
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
