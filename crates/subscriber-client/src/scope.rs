//! What the user chose to send into Mirage.
//!
//! One scope for SOCKS, HTTP, and TUN. A site is `HOST` or `HOST:PORT`.
//! An app is an exe file name or a full path. An empty list does not
//! constrain that side. When both sides are set, a flow is taken if the
//! site matches or the app matches. The host is never resolved here.

use std::net::IpAddr;
use std::sync::RwLock;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServiceRule {
    host: String,
    port: Option<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScopeBody {
    /// `None` = every destination.
    rules: Option<Vec<ServiceRule>>,
    /// `None` = every local process. Entries are slash-normalized and lowercased.
    apps: Option<Vec<String>>,
}

/// Shared by the listeners. `set_lists` is visible to tasks that already hold an `Arc`.
#[derive(Debug)]
pub struct EgressScope {
    inner: RwLock<ScopeBody>,
}

impl Clone for EgressScope {
    fn clone(&self) -> Self {
        let body = self.inner.read().unwrap_or_else(|e| e.into_inner()).clone();
        Self::from_body(body)
    }
}

impl EgressScope {
    pub fn all() -> Self {
        Self::from_body(ScopeBody { rules: None, apps: None })
    }

    fn from_body(body: ScopeBody) -> Self {
        Self { inner: RwLock::new(body) }
    }

    pub fn is_all(&self) -> bool {
        let body = self.inner.read().unwrap_or_else(|e| e.into_inner());
        body.rules.is_none() && body.apps.is_none()
    }

    /// Sites only. An empty slice is every destination and every app.
    pub fn from_services(specs: &[String]) -> Result<Self, String> {
        Self::from_lists(specs, &[])
    }

    /// `HOST` or `HOST:PORT` for a site. An app is a file name (`chrome.exe`)
    /// or a full path. Bracketed IPv6 is accepted.
    pub fn from_lists(services: &[String], apps: &[String]) -> Result<Self, String> {
        let body = parse_body(services, apps)?;
        Ok(Self::from_body(body))
    }

    /// Replace the lists in place. Listeners observe the next flow.
    pub fn set_lists(&self, services: &[String], apps: &[String]) -> Result<(), String> {
        let body = parse_body(services, apps)?;
        *self.inner.write().unwrap_or_else(|e| e.into_inner()) = body;
        Ok(())
    }

    /// Destination only. App constraints are applied by [`Self::allows_flow`].
    pub fn allows(&self, target: &str) -> bool {
        let body = self.inner.read().unwrap_or_else(|e| e.into_inner());
        dest_allowed(&body, target)
    }

    /// SOCKS, HTTP, and the same rule TUN split uses.
    ///
    /// `exe` is the full path of the process that connected, when known.
    /// A non-empty app list and an unknown process is refused.
    pub fn allows_flow(&self, target: &str, exe: Option<&str>) -> bool {
        let body = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let dest_on = body.rules.is_some();
        let app_on = body.apps.is_some();
        let dest_hit = dest_allowed(&body, target);
        let app_hit = app_allowed(&body, exe);
        match (dest_on, app_on) {
            (false, false) => true,
            (true, false) => dest_hit,
            (false, true) => app_hit,
            (true, true) => dest_hit || app_hit,
        }
    }
}

fn parse_body(services: &[String], apps: &[String]) -> Result<ScopeBody, String> {
    let rules = if services.is_empty() {
        None
    } else {
        let mut rules = Vec::with_capacity(services.len());
        for spec in services {
            let (host, port) = split_host_port(spec)?;
            if host.is_empty() {
                return Err(format!("empty host in --service {spec}"));
            }
            rules.push(ServiceRule { host: host.to_ascii_lowercase(), port });
        }
        Some(rules)
    };
    let apps = if apps.is_empty() {
        None
    } else {
        let mut out = Vec::with_capacity(apps.len());
        for spec in apps {
            let spec = spec.trim();
            if spec.is_empty() {
                return Err("empty app".into());
            }
            let norm = normalize_app(spec);
            if norm.is_empty() || norm.ends_with('/') {
                return Err(format!("empty app in --app {spec}"));
            }
            out.push(norm);
        }
        Some(out)
    };
    Ok(ScopeBody { rules, apps })
}

fn dest_allowed(body: &ScopeBody, target: &str) -> bool {
    let Some(rules) = &body.rules else {
        return true;
    };
    let Ok((host, port)) = split_host_port(target) else {
        return false;
    };
    let Some(port) = port else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    rules.iter().any(|rule| rule.host == host && rule.port.map(|p| p == port).unwrap_or(true))
}

fn app_allowed(body: &ScopeBody, exe: Option<&str>) -> bool {
    let Some(apps) = &body.apps else {
        return true;
    };
    let Some(exe) = exe else {
        return false;
    };
    let exe = normalize_app(exe);
    let file = exe.rsplit('/').next().unwrap_or(exe.as_str());
    apps.iter().any(|spec| spec == &exe || (!spec.contains('/') && spec == file))
}

fn normalize_app(path: &str) -> String {
    path.trim().to_ascii_lowercase().replace('\\', "/")
}

/// Host without brackets, plus an optional port. The host is not lowercased.
pub(crate) fn split_host_port(spec: &str) -> Result<(String, Option<u16>), String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("empty service".into());
    }
    if let Some(rest) = spec.strip_prefix('[') {
        let (addr, after) = rest
            .split_once(']')
            .ok_or_else(|| format!("unclosed IPv6 bracket in {spec}"))?;
        if addr.is_empty() {
            return Err(format!("empty host in {spec}"));
        }
        if after.is_empty() {
            return Ok((addr.to_string(), None));
        }
        let port_str = after
            .strip_prefix(':')
            .ok_or_else(|| format!("junk after IPv6 address in {spec}"))?;
        let port: u16 = port_str
            .parse()
            .map_err(|_| format!("bad port in {spec}"))?;
        return Ok((addr.to_string(), Some(port)));
    }
    if let Some((host, port_str)) = spec.rsplit_once(':') {
        if !host.is_empty() && !host.contains(':') && !port_str.is_empty() {
            if let Ok(port) = port_str.parse::<u16>() {
                return Ok((host.to_string(), Some(port)));
            }
        }
    }
    Ok((spec.to_string(), None))
}

/// Tun split rules name an IP by prefix or a domain. A port on the spec is
/// not expressible as host-and-port in tun policy (`DestPort` would catch
/// every flow on that port), so it is dropped here. SOCKS and HTTP still
/// enforce it.
pub(crate) fn tun_service_action(spec: &str) -> Result<(&'static str, String), String> {
    // A network is named as it is written: "10.0.0.0/8".  The device turns
    // such a rule into a hole in the routing, so the whole network leaves
    // the machine the way it always did.  A bare address is a host route.
    let trimmed = spec.trim();
    if let Some((addr, prefix)) = trimmed.split_once('/') {
        let addr = addr.trim();
        let prefix = prefix.trim();
        if addr.parse::<IpAddr>().is_ok() && prefix.parse::<u8>().is_ok() {
            return Ok(("dest_cidr", format!("{addr}/{prefix}")));
        }
    }
    let (host, _) = split_host_port(spec)?;
    if host.is_empty() {
        return Err(format!("empty host in --service {spec}"));
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        let cidr = match ip {
            IpAddr::V4(_) => format!("{ip}/32"),
            IpAddr::V6(_) => format!("{ip}/128"),
        };
        Ok(("dest_cidr", cidr))
    } else {
        Ok(("dest_domain", host.to_ascii_lowercase()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_allows_any_target() {
        let scope = EgressScope::all();
        assert!(scope.allows("api.ipify.org:443"));
        assert!(scope.allows("1.1.1.1:80"));
    }

    #[test]
    fn host_and_port_is_exact() {
        let scope = EgressScope::from_services(&["api.ipify.org:443".into()]).unwrap();
        assert!(scope.allows("api.ipify.org:443"));
        assert!(scope.allows("API.IPIFY.ORG:443"));
        assert!(!scope.allows("api.ipify.org:80"));
        assert!(!scope.allows("example.com:443"));
    }

    #[test]
    fn host_without_port_matches_any_port() {
        let scope = EgressScope::from_services(&["1.1.1.1".into()]).unwrap();
        assert!(scope.allows("1.1.1.1:80"));
        assert!(scope.allows("1.1.1.1:443"));
        assert!(!scope.allows("1.0.0.1:443"));
    }

    #[test]
    fn ipv6_literal_is_not_split_on_colons() {
        let scope = EgressScope::from_services(&["[2001:db8::1]:443".into()]).unwrap();
        assert!(scope.allows("[2001:db8::1]:443"));
        assert!(!scope.allows("[2001:db8::1]:80"));
        let bare = EgressScope::from_services(&["2001:db8::1".into()]).unwrap();
        assert!(bare.allows("[2001:db8::1]:53"));
    }

    #[test]
    fn tun_action_uses_prefix_or_domain_not_port() {
        assert_eq!(tun_service_action("1.1.1.1:443").unwrap(), ("dest_cidr", "1.1.1.1/32".into()));
        assert_eq!(
            tun_service_action("api.ipify.org:443").unwrap(),
            ("dest_domain", "api.ipify.org".into())
        );
    }

    #[test]
    fn tun_action_keeps_a_network_a_network() {
        assert_eq!(
            tun_service_action("2.17.251.112/32").unwrap(),
            ("dest_cidr", "2.17.251.112/32".into())
        );
        assert_eq!(
            tun_service_action("192.168.0.0/16").unwrap(),
            ("dest_cidr", "192.168.0.0/16".into())
        );
        // A name with a slash in it is still a name.
        assert_eq!(
            tun_service_action("example.com/path").unwrap(),
            ("dest_domain", "example.com/path".into())
        );
    }

    #[test]
    fn site_or_app_admits_either_side() {
        let scope = EgressScope::from_lists(
            &["api.ipify.org:443".into()],
            &["chrome.exe".into()],
        )
        .unwrap();
        let chrome = Some(r"C:\Program Files\Google\Chrome\Application\chrome.exe");
        let curl = Some(r"C:\Windows\System32\curl.exe");
        assert!(scope.allows_flow("api.ipify.org:443", curl));
        assert!(scope.allows_flow("example.com:80", chrome));
        assert!(!scope.allows_flow("example.com:80", curl));
        assert!(!scope.allows_flow("example.com:80", None));
    }

    #[test]
    fn app_alone_ignores_destination() {
        let scope = EgressScope::from_lists(&[], &["curl.exe".into()]).unwrap();
        assert!(scope.allows_flow("example.com:80", Some(r"C:\Windows\System32\curl.exe")));
        assert!(!scope.allows_flow("example.com:80", Some(r"C:\Windows\System32\curl2.exe")));
        assert!(!scope.allows_flow("example.com:80", None));
    }

    #[test]
    fn set_lists_is_visible_on_the_same_scope() {
        let scope = EgressScope::all();
        assert!(scope.allows_flow("example.com:80", None));
        scope.set_lists(&["example.com:443".into()], &[]).unwrap();
        assert!(scope.allows_flow("example.com:443", None));
        assert!(!scope.allows_flow("example.com:80", None));
    }
}
