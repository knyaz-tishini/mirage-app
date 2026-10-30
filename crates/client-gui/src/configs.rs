//! The person's own configurations.
//!
//! A configuration is one way out: a box the client assembled itself, or a
//! chain it was given. Each one keeps its entrance artefact in a directory of
//! its own, and the list of those directories lives in one file beside them.
//!
//! The list is what the GUI shows. How many configurations there are, there
//! are as many rows — the person never has to count them ahead of time.
//!
//! Nothing here knows what a configuration IS. The artefact says: `kind` of
//! `onion` is a chain the client walks itself, and no `kind` at all is the
//! classic one-box entrance. The GUI reads that field and shows it.

use std::path::PathBuf;

/// One configuration.
#[derive(Clone, Debug, Default)]
pub struct Config {
    /// The name the person sees.
    pub name: String,
    /// What the artefact says this configuration is: `vpn` or `onion`.
    pub kind: String,
    /// The directory this configuration's files live in.
    pub dir: PathBuf,
}

impl Config {
    /// The entrance this configuration is.
    pub fn artefact(&self) -> PathBuf {
        self.dir.join("artefact.json")
    }

    /// The text of the entrance, when it is still there.
    pub fn read(&self) -> Option<String> {
        std::fs::read_to_string(self.artefact()).ok()
    }

    /// The protocol this configuration speaks, in the person's words.
    ///
    /// The artefact says it: the entrance's camouflage IS the protocol — the
    /// QUIC carrier (M-KVIK), the WireGuard-like carrier (M-VG), a plain
    /// TCP face, and so on. A configuration that does not say is shown
    /// without one.
    pub fn protocol(&self) -> String {
        self.read().as_deref().and_then(read_protocol).unwrap_or_default()
    }
}

/// Where the app keeps what it made.
fn root() -> Option<PathBuf> {
    crate::backend::exe_dir().ok().map(|dir| dir.join("standalone"))
}

/// The directory the configurations live in.
fn configs_dir() -> Option<PathBuf> {
    Some(root()?.join("configs"))
}

/// The file the list of configurations lives in.
fn registry() -> Option<PathBuf> {
    Some(root()?.join("configs.json"))
}

/// A name that is safe as a directory and tells one configuration from another.
pub fn slug(text: &str) -> String {
    let mut out = String::new();
    for ch in text.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_owned();
    if out.is_empty() {
        return "config".to_owned();
    }
    out.chars().take(64).collect()
}

/// The directory one configuration's files live in.
pub fn dir_for(id: &str) -> Option<PathBuf> {
    Some(configs_dir()?.join(slug(id)))
}

/// Every configuration the client knows.
pub fn list() -> Vec<Config> {
    let Some(path) = registry() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(items) = value.get("configs").and_then(|item| item.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let name = item.get("name")?.as_str()?.trim().to_owned();
            let id = item.get("id")?.as_str()?.trim().to_owned();
            if name.is_empty() || id.is_empty() {
                return None;
            }
            Some(Config {
                name,
                kind: item
                    .get("kind")
                    .and_then(|kind| kind.as_str())
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
                dir: dir_for(&id)?,
            })
        })
        .collect()
}

/// Write the list down.
fn save(list: &[Config]) {
    let Some(path) = registry() else {
        return;
    };
    let items: Vec<serde_json::Value> = list
        .iter()
        .filter_map(|config| {
            let id = config.dir.file_name()?.to_str()?.to_owned();
            Some(serde_json::json!({
                "id": id,
                "name": config.name,
                "kind": config.kind,
            }))
        })
        .collect();
    let Ok(text) = serde_json::to_string_pretty(&serde_json::json!({ "configs": items })) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, text);
}

/// Put a configuration in the list, or refresh the one that is already there.
///
/// The id is the directory's name: the same id means the same configuration,
/// so a second install on the same box replaces the first one instead of
/// growing a second row.
pub fn put(name: &str, kind: &str, id: &str) -> Option<Config> {
    let dir = dir_for(id)?;
    let _ = std::fs::create_dir_all(&dir);
    let config = Config {
        name: name.trim().to_owned(),
        kind: kind.trim().to_owned(),
        dir,
    };
    let mut list = list();
    list.retain(|item| item.dir != config.dir);
    list.push(config.clone());
    save(&list);
    Some(config)
}

/// Forget a configuration. Its files stay on disk — the person may want them.
pub fn forget(config: &Config) {
    let mut list = list();
    let before = list.len();
    list.retain(|item| item.dir != config.dir);
    if list.len() != before {
        save(&list);
    }
}

/// An id that no configuration has taken yet.
fn free_id(wanted: &str) -> String {
    let base = slug(wanted);
    let taken = list();
    if !taken.iter().any(|item| item.dir.file_name().is_some_and(|name| name == base.as_str())) {
        return base;
    }
    for index in 2..1000 {
        let candidate = format!("{base}-{index}");
        if !taken
            .iter()
            .any(|item| item.dir.file_name().is_some_and(|name| name == candidate.as_str()))
        {
            return candidate;
        }
    }
    base
}

/// What the artefact says it is, and what it is called.
///
/// The protocol an artefact speaks, in the person's words.
///
/// The entrance's camouflage is the protocol: the QUIC carrier is M-KVIK,
/// the WireGuard-like carrier is M-VG, and a face without a mask is a plain
/// TCP face. A configuration whose artefact says nothing is shown without a
/// protocol rather than with a guess.
pub fn read_protocol(artefact: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(artefact).ok()?;
    let kind = value
        .get("entrances")
        .and_then(|list| list.as_array())
        .and_then(|list| list.first())
        .and_then(|entrance| entrance.get("camouflage"))
        .and_then(|mask| mask.get("type"))
        .and_then(|kind| kind.as_str())?;
    Some(
        match kind {
            "quic" => "M-KVIK",
            "wire_guard" => "M-VG",
            "none" => "TCP",
            "tls_real" => "M-TLS",
            "http3" => "M-H3",
            "ssh" => "M-SSH",
            "websocket" => "M-WS",
            "grpc" => "M-GRPC",
            "reality" => "M-REALITY",
            "webtransport" => "M-WT",
            "xhttp" => "M-XHTTP",
            "raw_udp" => "M-UDP",
            "icmp" => "M-ICMP",
            "dns_tunnel" => "M-DNS",
            "pipeline" => "M-PIPE",
            other => other,
        }
        .to_owned(),
    )
}

/// The kind is the artefact's own word: `onion` is a chain, and the absence
/// of the word is the classic one-box entrance.
pub fn read_name(artefact: &str) -> (String, String) {    let Ok(value) = serde_json::from_str::<serde_json::Value>(artefact) else {
        return (String::new(), String::new());
    };
    let kind = match value.get("kind").and_then(|item| item.as_str()) {
        Some(kind) if kind.eq_ignore_ascii_case("onion") => "onion".to_owned(),
        _ => "vpn".to_owned(),
    };
    let addr = if kind == "onion" {
        value
            .get("hops")
            .and_then(|hops| hops.as_array())
            .and_then(|hops| hops.last())
            .and_then(|hop| hop.get("addr"))
            .and_then(|addr| addr.as_str())
            .map(str::to_owned)
    } else {
        value
            .get("entrances")
            .and_then(|list| list.as_array())
            .and_then(|list| list.first())
            .and_then(|entrance| entrance.get("addr"))
            .and_then(|addr| addr.as_str())
            .map(str::to_owned)
    };
    let host = match addr {
        Some(addr) => match addr.rsplit_once(':') {
            Some((host, _)) => host.to_owned(),
            None => addr,
        },
        None => String::new(),
    };
    let label = value
        .get("label")
        .and_then(|item| item.as_str())
        .unwrap_or_default()
        .trim()
        .to_owned();
    let name = if !host.is_empty() {
        host
    } else if !label.is_empty() {
        label
    } else {
        String::new()
    };
    (name, kind)
}

/// The configuration that already carries this artefact, if there is one.
pub fn find_by_artefact(artefact: &str) -> Option<Config> {
    list()
        .into_iter()
        .find(|config| config.read().as_deref() == Some(artefact))
}

/// Take an artefact into the list as a configuration.
///
/// The text is written into a directory of its own, and the row appears in
/// the list of what one can connect to.
pub fn take_artefact(artefact: &str, wanted: &str) -> Option<Config> {
    let (name, kind) = read_name(artefact);
    let name = if wanted.trim().is_empty() { name } else { wanted.trim().to_owned() };
    let id = free_id(if name.is_empty() { "config" } else { &name });
    let dir = dir_for(&id)?;
    let _ = std::fs::create_dir_all(&dir);
    std::fs::write(dir.join("artefact.json"), artefact).ok()?;
    put(&name, &kind, &id)
}

/// The configuration of a box the client just assembled.
///
/// The box's address is the name: two boxes are two rows, and the same box
/// installed twice is the same row.
pub fn take_install(host: &str) -> Option<Config> {
    let id = slug(host);
    let dir = dir_for(&id)?;
    let kind = std::fs::read_to_string(dir.join("artefact.json"))
        .map(|text| read_name(&text).1)
        .unwrap_or_else(|_| "vpn".to_owned());
    put(host, &kind, &id)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The classic entrance has no `kind` at all — the discriminant is additive.
    #[test]
    fn the_vpn_artefact_has_no_kind_and_the_onion_one_has() {
        let vpn = r#"{"v":1,"entrances":[{"addr":"edge.example:41443"}],"label":"edge"}"#;
        let (name, kind) = read_name(vpn);
        assert_eq!(kind, "vpn");
        assert_eq!(name, "edge.example");

        let onion = r#"{"v":1,"kind":"onion","hops":[{"addr":"hop.example:443"},{"addr":"exit.example:443"}]}"#;
        let (name, kind) = read_name(onion);
        assert_eq!(kind, "onion");
        assert_eq!(name, "exit.example");
    }

    #[test]
    fn a_slug_is_safe_as_a_directory() {
        assert_eq!(slug("144.172.105.29"), "144.172.105.29");
        assert_eq!(slug("a b/c"), "a-b-c");
        assert_eq!(slug(""), "config");
        assert_eq!(slug("../../etc"), "etc");
    }
}
