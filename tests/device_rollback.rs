use std::process::Command;

#[test]
fn name_only_rollback_preserves_a_device_with_the_same_id() {
    let root = std::env::temp_dir().join(format!(
        "ocu-device-rollback-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let config = if cfg!(target_os = "macos") {
        root.join("Library/Application Support/OpenComputerUse")
    } else if cfg!(windows) {
        root.join("OpenComputerUse")
    } else {
        root.join("opencomputeruse")
    };
    std::fs::create_dir_all(&config).unwrap();
    let path = config.join("devices.json");
    let original = serde_json::json!({
        "id": "deadbeef", "name": "existing-user", "url": "http://localhost:8642",
        "key_hash": "original-key-hash", "created": 1
    });
    std::fs::write(
        &path,
        serde_json::json!({"devices": [original.clone()]}).to_string(),
    )
    .unwrap();
    let run = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_opencomputeruse"))
            .args(args)
            .env("HOME", &root)
            .env("APPDATA", &root)
            .env("XDG_CONFIG_HOME", &root)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };
    run(&[
        "device",
        "add",
        "--name",
        "deadbeef",
        "--url",
        "http://localhost:18642",
    ]);
    let added: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(added["devices"].as_array().unwrap().len(), 2);
    run(&["device", "remove", "--name", "deadbeef"]);
    let remaining: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(remaining["devices"], serde_json::json!([original]));
    run(&["device", "remove", "deadbeef"]);
    let remaining: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(remaining["devices"], serde_json::json!([]));
}
