//! Hardcoded-creds sentinel smoke-test binary.
//!
//! Run on a fresh user machine to verify we can reach the monitor.
//! Does NOT spawn cli, tun, or anything else.  Just registers the client
//! and pushes heartbeats / a synthetic crash report so the operator
//! sees the box light up in `/relay/clients`.
//!
//! Builds the same Reqwest stack as the real sentinel; if this binary
//! gets through but the GUI launcher does not, the problem is in the
//! GUI / wintun / Defender — not in the network path.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MONITOR_URLS: &[&str] = &[
    "https://lm.zen-geldmaschine.net:19443",
    "https://lm-nl.zen-geldmaschine.net:19443",
];
const API_KEY:    &str = "ae08d625-6a22-48a5-b175-65e011d76f16";
const CLIENT_ID:  &str = "tester2";

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Console + file logging so the user sees something on screen even
    // if Defender quarantines the binary mid-run.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    println!("=== mirage-sentinel-test ===");
    println!("monitors:");
    for u in MONITOR_URLS { println!("  - {u}"); }
    println!("client  : {}", CLIENT_ID);
    println!();

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("FATAL: reqwest client build failed: {e}");
            wait_and_exit(1);
            return;
        }
    };

    // Step 1: race /health across all URLs, pick first that answers 2xx.
    let mut monitor_url: Option<&str> = None;
    for u in MONITOR_URLS {
        print!("[1/3] GET {u}/health ... ");
        match client.get(format!("{u}/health")).send().await {
            Ok(r) if r.status().is_success() => {
                let body = r.text().await.unwrap_or_default();
                println!("OK -> {}", body.trim().chars().take(80).collect::<String>());
                monitor_url = Some(*u);
                break;
            }
            Ok(r) => println!("HTTP {} (try next)", r.status()),
            Err(e) => println!("FAIL ({e}) (try next)"),
        }
    }
    let monitor_url = match monitor_url {
        Some(u) => u,
        None => {
            eprintln!("all monitors unreachable");
            wait_and_exit(1);
            return;
        }
    };
    println!("active monitor: {monitor_url}");

    // Step 2: POST /relay/clients/<id>/heartbeat (public — no auth, anyone
    // with a client_id can ping).  Confirms client_id mapping works.
    print!("[2/3] POST /relay/clients/{CLIENT_ID}/heartbeat ... ");
    let hb_url = format!("{monitor_url}/relay/clients/{CLIENT_ID}/heartbeat");
    match client.post(&hb_url)
        .json(&serde_json::json!({
            "client_id": CLIENT_ID,
            "version": env!("CARGO_PKG_VERSION"),
            "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        }))
        .send().await
    {
        Ok(r) if r.status().is_success() => println!("OK ({})", r.status()),
        Ok(r) => println!("HTTP {} (treated as soft-fail)", r.status()),
        Err(e) => {
            println!("FAIL");
            eprintln!("heartbeat error: {e}");
        }
    }

    // Step 3: POST /relay/clients/<id>/crash with a synthetic "hello" report
    // so the operator sees us in the crash feed.
    print!("[3/3] POST /relay/clients/{CLIENT_ID}/crash ... ");
    let now_ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
    let crash_url = format!("{monitor_url}/relay/clients/{CLIENT_ID}/crash");
    let crash_body = serde_json::json!({
        "process": "sentinel-test",
        "kind": "other",
        "exit_code": 0,
        "started_at_ms": now_ms,
        "died_at_ms": now_ms,
        "uptime_ms": 0u64,
        "stderr_tail": format!(
            "synthetic hello from mirage-sentinel-test on {}-{} (host: {:?})",
            std::env::consts::OS,
            std::env::consts::ARCH,
            std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_default(),
        ),
        "stdout_tail": "",
        "version": env!("CARGO_PKG_VERSION"),
        "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
    });
    match client.post(&crash_url)
        .bearer_auth(API_KEY)
        .json(&crash_body)
        .send().await
    {
        Ok(r) if r.status().is_success() => println!("OK ({})", r.status()),
        Ok(r) => {
            println!("HTTP {}", r.status());
            let t = r.text().await.unwrap_or_default();
            eprintln!("crash post body: {}", t);
        }
        Err(e) => {
            println!("FAIL");
            eprintln!("crash post error: {e}");
        }
    }

    println!();
    println!("=== DONE — check operator dashboard or:");
    println!("    GET {monitor_url}/relay/crashes?client_id={CLIENT_ID}&limit=1");
    wait_and_exit(0);
}

fn wait_and_exit(code: i32) {
    println!("\nPress ENTER to close this window...");
    let mut s = String::new();
    let _ = std::io::stdin().read_line(&mut s);
    std::process::exit(code);
}
