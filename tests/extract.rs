//! End to end: `opencomputeruse extract` starts a real app showing an
//! invoice (`tests/e2e/*/invoice*`), reads its window's accessibility tree
//! and screenshot, and Cloudflare's Clairic answers typed questions about
//! it. Text answers must name the element they came from.
//!
//! It calls Workers AI, so it only runs when `OCU_E2E_INVOICE_APP` names the
//! test app and `CLOUDFLARE_ACCOUNT_ID` and `CLOUDFLARE_API_TOKEN` are set;
//! `.github/workflows/e2e.yml` sets them up when the repository has the
//! secrets.

use std::process::Command;
use std::sync::Mutex;

use serde_json::{json, Value};

/// What the test app shows, and so what Clairic must read.
const INVOICE_NUMBER: &str = "INV-2291";
const TOTAL: f64 = 1080.0;
const DUE: &str = "2026-11-01";

fn setup() -> Option<String> {
    let app = std::env::var("OCU_E2E_INVOICE_APP").ok()?;
    let creds = ["CLOUDFLARE_ACCOUNT_ID", "CLOUDFLARE_API_TOKEN"]
        .iter()
        .all(|k| std::env::var(k).is_ok_and(|v| !v.is_empty()));
    if !creds {
        eprintln!("extract: no Cloudflare credentials; skipping");
        return None;
    }
    Some(app)
}

/// Runs `extract` on the app with `fields` and `extra` flags, and returns
/// its answer. Once more if the run itself fails (the network, a slow
/// start); a wrong answer is not retried.
fn extract(app: &str, fields: &Value, extra: &[&str]) -> Value {
    // One app at a time: a second launch on macOS attaches to the first,
    // which the first session's end then quits.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let config = std::env::temp_dir().join(format!("ocu-e2e-extract-{}.json", std::process::id()));
    let mut last = String::new();
    for attempt in 1..=2 {
        let out = Command::new(env!("CARGO_BIN_EXE_opencomputeruse"))
            .args(["extract", "--app", app, "--wait", "4000", "--fields"])
            .arg(fields.to_string())
            .args(extra)
            // The app is driven here, not through the macOS agent, and no
            // saved settings get in the way.
            .env("OCU_IN_PROCESS", "1")
            .env("OCU_CONFIG", &config)
            .env(
                "OCU_LOG",
                std::env::var("OCU_LOG").unwrap_or_else(|_| "info".into()),
            )
            .output()
            .expect("running opencomputeruse");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        eprintln!("--- extract {extra:?} (attempt {attempt}) ---\n{stderr}");
        if out.status.success() {
            println!("{stdout}");
            return serde_json::from_str(&stdout).unwrap_or_else(|e| {
                panic!("extract printed something other than JSON ({e}): {stdout}")
            });
        }
        last = format!("{}: {stderr}", out.status);
    }
    panic!("extract failed twice; last: {last}");
}

fn number(v: &Value) -> f64 {
    v.as_f64()
        .unwrap_or_else(|| panic!("expected a number, got {v}"))
}

#[test]
fn reads_an_invoice_from_its_tree_and_screenshot() {
    let Some(app) = setup() else { return };
    let fields = json!({
        "invoice_number": { "type": "text", "instructions": "The invoice number" },
        "total": { "type": "number", "decimals": 2, "unit": "USD", "instructions": "Total amount due" },
        "due": { "type": "date", "instructions": "When payment is due" },
        "send_button": { "type": "text", "instructions": "The label of the button that sends the invoice" },
        "po_number": { "type": "text", "instructions": "The purchase order number" },
    });
    let out = extract(&app, &fields, &[]);
    let read = &out["read"];
    assert!(
        read["tree_elements"].as_u64().unwrap_or(0) > 0,
        "the tree was not read: {out:#}"
    );
    assert!(
        read["screenshot"].is_string(),
        "no screenshot was sent: {out:#}"
    );

    let a = &out["answers"];
    assert_eq!(a["invoice_number"]["value"], INVOICE_NUMBER, "{out:#}");
    assert!(
        (number(&a["total"]["value"]) - TOTAL).abs() < 0.01,
        "{out:#}"
    );
    assert_eq!(a["due"]["value"], DUE, "{out:#}");
    let send = a["send_button"]["value"].as_str().unwrap_or_default();
    assert!(send.contains("Send"), "send_button: {out:#}");
    // The invoice shows no purchase order.
    assert!(a["po_number"]["value"].is_null(), "po_number: {out:#}");

    // Text answers name the tree element they were read from, each its own.
    let element = |k: &str| {
        a[k]["element"]
            .as_str()
            .unwrap_or_else(|| panic!("{k} names no element: {out:#}"))
            .to_string()
    };
    let (label, button) = (element("invoice_number"), element("send_button"));
    assert_ne!(label, button, "{out:#}");
}

#[test]
fn reads_an_invoice_from_its_screenshot_alone() {
    let Some(app) = setup() else { return };
    let fields = json!({
        "total": { "type": "number", "decimals": 2, "unit": "USD", "instructions": "Total amount due" },
        "due": { "type": "date", "instructions": "When payment is due" },
    });
    let out = extract(&app, &fields, &["--no-tree"]);
    assert_eq!(out["read"]["tree_elements"], 0, "{out:#}");
    assert!(out["read"]["screenshot"].is_string(), "{out:#}");
    let a = &out["answers"];
    assert!(
        (number(&a["total"]["value"]) - TOTAL).abs() < 0.01,
        "{out:#}"
    );
    assert_eq!(a["due"]["value"], DUE, "{out:#}");
}
