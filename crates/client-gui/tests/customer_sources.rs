//! Customer-build source gate (launch plan D11).
//!
//! The customer binary must carry no operator, fleet or demo content and no
//! developer control endpoint. This test scans every file under `src/` EXCEPT
//! `src/dev/` (the `dev-tools` feature, off by default) for the denylist below
//! and fails with `path:line: reason` for every hit.
//!
//! Denylist classes:
//!   * hoster names (the fleet's providers and common others),
//!   * the fleet/demo data identifiers (`FLEET`, `LINKS`, `OFFERS`, `MEMBERS`,
//!     `FleetNode`, `NodeOffer`) as whole words,
//!   * the developer control surface (`agent_api`) and the operator/demo CLI
//!     flags (`--admin`, `--no-buy`, `--no-manual-exit`),
//!   * the operator DNS zone (`zen-geldmaschine`),
//!   * staging ids (anything containing `staging`),
//!   * any IPv4 literal except `127.0.0.1`.

use std::fs;
use std::path::{Path, PathBuf};

/// Case-insensitive substrings that must not appear in customer sources.
const FORBIDDEN_SUBSTRINGS: &[(&str, &str)] = &[
    ("hostkey", "hoster name"),
    ("cloudzy", "hoster name"),
    ("hosthatch", "hoster name"),
    ("hetzner", "hoster name"),
    ("vultr", "hoster name"),
    ("digitalocean", "hoster name"),
    ("linode", "hoster name"),
    ("contabo", "hoster name"),
    ("aeza", "hoster name"),
    ("ishosting", "hoster name"),
    ("agent_api", "developer control surface"),
    ("--admin", "operator flag"),
    ("--no-buy", "demo flag"),
    ("--no-manual-exit", "demo flag"),
    ("zen-geldmaschine", "operator DNS zone"),
    ("staging", "staging id"),
];

/// Case-sensitive whole-word identifiers that must not appear.
const FORBIDDEN_WORDS: &[(&str, &str)] = &[
    ("FLEET", "fleet data"),
    ("LINKS", "fleet data"),
    ("OFFERS", "demo data"),
    ("MEMBERS", "demo data"),
    ("FleetNode", "fleet data"),
    ("NodeOffer", "demo data"),
];

/// The only IPv4 literal a customer source may carry.
const LOOPBACK: &str = "127.0.0.1";

fn src_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// Every scanned file under `src/`, `src/dev/` excluded, sorted.
fn customer_source_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, dev_dir: &Path, out: &mut Vec<PathBuf>) {
        let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
        for entry in entries {
            let path = entry
                .unwrap_or_else(|e| panic!("dir entry in {}: {e}", dir.display()))
                .path();
            if path.is_dir() {
                if path != dev_dir {
                    walk(&path, dev_dir, out);
                }
            } else {
                out.push(path);
            }
        }
    }

    let root = src_dir();
    let dev_dir = root.join("dev");
    let mut out = Vec::new();
    walk(&root, &dev_dir, &mut out);
    out.sort();
    out
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// True when `word` occurs in `line` bounded by non-word characters.
fn contains_word(line: &str, word: &str) -> bool {
    line.match_indices(word).any(|(i, _)| {
        let before_ok = line[..i].chars().next_back().map_or(true, |c| !is_word_char(c));
        let after_ok = line[i + word.len()..].chars().next().map_or(true, |c| !is_word_char(c));
        before_ok && after_ok
    })
}

/// Every dotted-quad IPv4 literal on `line` (each run of digits and dots that
/// splits into exactly four numeric octets, each 0..=255).
fn ipv4_literals(line: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut run = String::new();
    // A trailing sentinel flushes the last run.
    for c in line.chars().chain(std::iter::once(' ')) {
        if c.is_ascii_digit() || c == '.' {
            run.push(c);
            continue;
        }
        let candidate = run.trim_matches('.');
        let octets: Vec<&str> = candidate.split('.').collect();
        let is_quad = octets.len() == 4
            && octets.iter().all(|o| {
                !o.is_empty() && o.len() <= 3 && o.parse::<u16>().map_or(false, |n| n <= 255)
            });
        if is_quad {
            found.push(candidate.to_string());
        }
        run.clear();
    }
    found
}

/// Denylist hits on one line, as human-readable reasons.
fn line_violations(line: &str) -> Vec<String> {
    let lower = line.to_lowercase();
    let mut hits = Vec::new();
    for (needle, why) in FORBIDDEN_SUBSTRINGS {
        if lower.contains(needle) {
            hits.push(format!("`{needle}` ({why})"));
        }
    }
    for (word, why) in FORBIDDEN_WORDS {
        if contains_word(line, word) {
            hits.push(format!("`{word}` ({why})"));
        }
    }
    for ip in ipv4_literals(line) {
        if ip != LOOPBACK {
            hits.push(format!("IPv4 literal `{ip}` (only {LOOPBACK} is allowed)"));
        }
    }
    hits
}

#[test]
fn customer_sources_have_no_operator_strings() {
    let mut report = Vec::new();
    for path in customer_source_files() {
        // Binary assets (the baked land mask) carry no source text.
        let Ok(text) = fs::read_to_string(&path) else { continue };
        for (idx, line) in text.lines().enumerate() {
            for why in line_violations(line) {
                let rel = path.strip_prefix(src_dir()).unwrap_or(&path);
                report.push(format!("src/{}:{}: {why}", rel.display(), idx + 1));
            }
        }
    }
    assert!(
        report.is_empty(),
        "customer sources carry operator/fleet/demo content:\n{}",
        report.join("\n")
    );
}

#[test]
fn scan_covers_customer_sources_and_skips_dev() {
    let root = src_dir();
    let dev_dir = root.join("dev");
    let files = customer_source_files();
    for expected in ["main.rs", "app.rs", "nodes.rs", "backend.rs"] {
        assert!(
            files.contains(&root.join(expected)),
            "scan is missing src/{expected}"
        );
    }
    assert!(
        files.iter().any(|f| f.ends_with("shell/menu.rs")),
        "scan does not recurse into src/shell"
    );
    assert!(
        files.iter().all(|f| !f.starts_with(&dev_dir)),
        "scan must exclude src/dev"
    );
}

#[test]
fn denylist_matcher_flags_every_class() {
    let flagged = [
        "prov: \"HostKey\"",
        "prov: \"cloudzy\"",
        "FLEET.iter()",
        "for o in OFFERS {",
        "static MEMBERS: &[Member]",
        "static LINKS: &[(usize, usize)]",
        "builder.agent_api(1)",
        "args.any(|a| a == \"--admin\")",
        "\"--no-manual-exit\"",
        "lm-nl.zen-geldmaschine.net",
        "\"mesh_id\": \"v3-staging\"",
        "let ip = \"10.0.0.7:443\";",
    ];
    for line in flagged {
        assert!(!line_violations(line).is_empty(), "not flagged: {line}");
    }
}

#[test]
fn denylist_matcher_lets_customer_text_through() {
    let clean = [
        "const SENTINEL: &str = \"http://127.0.0.1:19420\";",
        "let x = 0.45 * wave + 1.5;",
        "// the planet skin draws CATALOG nodes",
        "let members_len = 3; // lowercase identifier, not the data table",
        "let flinks = 1;",
        "version 1.2.3",
    ];
    for line in clean {
        assert!(line_violations(line).is_empty(), "wrongly flagged: {line}");
    }
}

#[test]
fn ipv4_scanner_reads_octets_strictly() {
    assert_eq!(ipv4_literals("bind 127.0.0.1:19420"), vec!["127.0.0.1"]);
    assert_eq!(ipv4_literals("a 1.2.3.4 b 5.6.7.8."), vec!["1.2.3.4", "5.6.7.8"]);
    assert!(ipv4_literals("1.2.3").is_empty());
    assert!(ipv4_literals("1.2.3.4.5").is_empty());
    assert!(ipv4_literals("999.1.1.1").is_empty());
}

#[test]
fn dev_tools_is_not_a_default_feature() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let text = fs::read_to_string(&manifest)
        .unwrap_or_else(|e| panic!("read {}: {e}", manifest.display()));
    let features = text
        .split("[features]")
        .nth(1)
        .map(|rest| rest.split("\n[").next().unwrap_or(rest))
        .expect("Cargo.toml has a [features] table");
    let default_line = features
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("default"))
        .expect("[features] declares `default`");
    assert!(
        default_line.replace(' ', "") == "default=[]",
        "dev-tools must stay off by default, found: {default_line}"
    );
    assert!(
        features.lines().any(|l| l.trim().starts_with("dev-tools")),
        "[features] must declare `dev-tools`"
    );
}
