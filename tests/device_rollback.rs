use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A config folder of its own, with `devices.json` holding `devices`.
fn home(label: &str, devices: serde_json::Value) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "ocu-device-{label}-{}-{}",
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
    std::fs::write(&path, serde_json::json!({ "devices": devices }).to_string()).unwrap();
    (root, path)
}

fn ocu(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_opencomputeruse"))
        .args(args)
        .env("HOME", root)
        .env("APPDATA", root)
        .env("XDG_CONFIG_HOME", root)
        .output()
        .unwrap()
}

fn read(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

#[test]
fn name_only_rollback_preserves_a_device_with_the_same_id() {
    let original = serde_json::json!({
        "id": "deadbeef", "name": "existing-user", "url": "http://localhost:8642",
        "key_hash": "original-key-hash", "created": 1
    });
    let (root, path) = home("rollback", serde_json::json!([original.clone()]));
    let run = |args: &[&str]| {
        let out = ocu(&root, args);
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
    assert_eq!(read(&path)["devices"].as_array().unwrap().len(), 2);
    run(&["device", "remove", "--name", "deadbeef"]);
    assert_eq!(read(&path)["devices"], serde_json::json!([original]));
    run(&["device", "remove", "deadbeef"]);
    assert_eq!(read(&path)["devices"], serde_json::json!([]));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_taken_name_keeps_its_key_unless_replaced() {
    let original = serde_json::json!({
        "id": "deadbeef", "name": "Laptop", "url": "http://localhost:8642",
        "key_hash": "original-key-hash", "created": 1
    });
    let (root, path) = home("taken", serde_json::json!([original.clone()]));
    let add = |extra: &[&str]| {
        let mut args = vec![
            "device",
            "add",
            "--name",
            "laptop",
            "--url",
            "http://127.0.0.1:18642",
        ];
        args.extend(extra);
        ocu(&root, &args)
    };

    let out = add(&[]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("ocu-device: exists"));
    assert!(out.stdout.is_empty());
    assert_eq!(read(&path)["devices"], serde_json::json!([original]));

    let out = add(&["--replace"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("ocu-device: regenerated"));
    assert!(String::from_utf8_lossy(&out.stdout)
        .trim()
        .starts_with("ocu_"));
    let devices = read(&path)["devices"].clone();
    assert_eq!(devices.as_array().unwrap().len(), 1);
    assert_ne!(devices[0]["key_hash"], "original-key-hash");

    let out = ocu(&root, &["device", "list"]);
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "Laptop\thttp://127.0.0.1:18642\tdeadbeef\n"
    );
    let _ = std::fs::remove_dir_all(&root);
}
