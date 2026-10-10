//! What the user chose to send into Mirage.
//!
//! Absent rules means every destination. A rule is a host, or a host and a
//! port. The host is a domain or an IP and is never resolved here. Where the
//! flow goes after it is allowed is the artefact's decision, not this list.

use std::net::IpAddr;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ServiceRule {
    host: String,
    port: Option<u16>,
}

/// `None` rules = every destination. `Some` = only the listed services.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressScope {
    rules: Option<Vec<ServiceRule>>,
}

impl EgressScope {
    pub fn all() -> Self {
        Self { rules: None }
    }

    pub fn is_all(&self) -> bool {
        self.rules.is_none()
    }

    /// `HOST` or `HOST:PORT`. `HOST` is a domain or an IP. Bracketed IPv6
    /// (`[2001:db8::1]`, `[2001:db8::1]:443`) is accepted. An unbracketed
    /// IPv6 literal is a host with no port — the colons are not a port
    /// separator.
    pub fn from_services(specs: &[String]) -> Result<Self, String> {
        if specs.is_empty() {
            return Ok(Self::all());
        }
        let mut rules = Vec::with_capacity(specs.len());
        for spec in specs {
            let (host, port) = split_host_port(spec)?;
            if host.is_empty() {
                return Err(format!("empty host in --service {spec}"));
            }
            rules.push(ServiceRule { host: host.to_ascii_lowercase(), port });
        }
        Ok(Self { rules: Some(rules) })
    }

    /// `target` is `host:port` as SOCKS and HTTP CONNECT already parsed it,
    /// including `[ipv6]:port`.
    pub fn allows(&self, target: &str) -> bool {
        let Some(rules) = &self.rules else {
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
}
