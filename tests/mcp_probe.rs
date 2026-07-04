use std::process::Command;

#[test]
fn mcp_probe_human_output_smokes_server() {
    let output = Command::new(env!("CARGO_BIN_EXE_prview"))
        .args(["mcp", "--probe"])
        .output()
        .expect("run prview mcp --probe");

    assert!(
        output.status.success(),
        "probe failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("prview mcp probe ok"));
    assert!(stdout.contains(&format!("version: {}", env!("CARGO_PKG_VERSION"))));
    assert!(stdout.contains("schema_version: prview.mcp.v1"));
    assert!(stdout.contains("tools: 6"));
    assert!(stdout.contains("response_ms:"));
}

#[test]
fn mcp_probe_json_output_is_machine_readable() {
    let output = Command::new(env!("CARGO_BIN_EXE_prview"))
        .args(["mcp", "--probe", "--json"])
        .output()
        .expect("run prview mcp --probe --json");

    assert!(
        output.status.success(),
        "probe json failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let payload: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("probe stdout is JSON");
    assert_eq!(payload["ok"], true);
    assert_eq!(payload["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(payload["schema_version"], "prview.mcp.v1");
    assert_eq!(payload["tools"], 6);
    assert!(payload["response_ms"].as_u64().is_some());
}
