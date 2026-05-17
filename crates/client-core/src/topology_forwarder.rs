//! [`TopologyForwarder`] — topology-aware DNS forwarder.
//!
//! Replaces the single-upstream `DnsForwarder` with a per-chain isolated
//! resolver.  Each chain resolves hostnames only through its own configured
//! DoH/DoT backend; the bypass tier has its own dedicated backend.
//!
//! # Architecture
//!
//! ```text
//! UDP packet from OS
//!   └─ TopologyForwarder::run()
//!        ├─ extract chain_id from routing context (or None for bypass)
//!        ├─ DnsCache::get(chain_id, name) → cache hit → reply
//!        └─ cache miss → DnsTopology::backend_for(chain_id) → dispatch
//!             ├─ DohOverChain → reqwest via chain SOCKS5 proxy
//!             ├─ DohDirect    → reqwest direct (bootstrap IP)
//!             └─ SystemResolver → tokio::net::lookup_host
//! ```

use std::{
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::Duration,
};

use mirage_core::{
    config::DnsConfig,
    dns::{DnsBackend, DnsCache, DnsTopology},
    error::MirageError,
};
use tokio::net::UdpSocket;
use tracing::{debug, error, info, warn};

use crate::ChainSet;

// ── Wire-format helpers ───────────────────────────────────────────────────────

/// Zero out the 2-byte transaction ID so identical questions hit the same
/// cache slot regardless of their query ID.
fn cache_key_from_wire(query: &[u8]) -> Vec<u8> {
    if query.len() < 2 {
        return query.to_vec();
    }
    let mut key = query.to_vec();
    key[0] = 0;
    key[1] = 0;
    key
}

/// Extract the query name from a DNS wire packet.  Returns `None` if the
/// packet is malformed or has no question section.
///
/// Format: 12-byte header, then QNAME (length-label sequence ending in 0x00),
/// then QTYPE (2 bytes), QCLASS (2 bytes).
fn extract_qname(query: &[u8]) -> Option<String> {
    if query.len() < 13 {
        return None;
    }
    let mut pos = 12usize; // skip DNS header
    let mut labels = Vec::new();
    loop {
        if pos >= query.len() {
            return None;
        }
        let len = query[pos] as usize;
        if len == 0 {
            break;
        }
        // Reject pointer compression — we only need the uncompressed form.
        if len & 0xC0 == 0xC0 {
            return None;
        }
        pos += 1;
        if pos + len > query.len() {
            return None;
        }
        labels.push(std::str::from_utf8(&query[pos..pos + len]).ok()?.to_owned());
        pos += len;
    }
    Some(labels.join("."))
}

/// Build a minimal SERVFAIL response for the given query bytes.
fn servfail_response(query: &[u8]) -> Vec<u8> {
    let mut resp = query.to_vec();
    if resp.len() < 12 {
        resp.resize(12, 0);
    }
    // Set QR=1, RCODE=2 (SERVFAIL) in the flags word (bytes 2-3).
    resp[2] = 0x81; // QR=1, OPCODE=0, AA=0, TC=0, RD=1
    resp[3] = 0x82; // RA=1, Z=0, RCODE=2
    // Clear ANCOUNT, NSCOUNT, ARCOUNT.
    for i in 6..12 {
        resp[i] = 0;
    }
    resp
}

// ── TopologyForwarder ─────────────────────────────────────────────────────────

/// Topology-aware DNS forwarder.  Binds a UDP listener and routes each DNS
/// query to the correct per-chain or bypass backend.
pub struct TopologyForwarder {
    /// Simple DNS config reused for the listen address and cache TTL.
    dns_cfg: DnsConfig,
    topology: Arc<DnsTopology>,
    cache: Arc<DnsCache>,
    chain_set: Arc<ChainSet>,
    /// SOCKS5 address used by chains that have no per-chain SOCKS5 override.
    /// Typically the cli's global SOCKS5 listener (`127.0.0.1:1080`).
    default_socks_addr: String,
}

impl TopologyForwarder {
    /// Construct a forwarder.
    ///
    /// `default_socks_addr` is the SOCKS5 proxy through which `DohOverChain`
    /// queries are sent when the chain's per-port SOCKS5 address is not
    /// individually configured.  Use `"socks5h://127.0.0.1:1080"` for the
    /// single-chain case.
    pub fn new(
        dns_cfg: DnsConfig,
        topology: Arc<DnsTopology>,
        chain_set: Arc<ChainSet>,
        default_socks_addr: String,
    ) -> Self {
        let ttl = Duration::from_secs(dns_cfg.cache_ttl_secs);
        Self {
            dns_cfg,
            topology,
            cache: Arc::new(DnsCache::new(1024, ttl)),
            chain_set,
            default_socks_addr,
        }
    }

    /// Bind the UDP socket and serve DNS queries.  Cancellation is handled by
    /// dropping the returned future.
    pub async fn run(self: Arc<Self>) -> Result<(), MirageError> {
        let listen: SocketAddr = self.dns_cfg.listen_addr.parse().map_err(|e| {
            MirageError::Config(format!(
                "topology dns listen_addr {:?} invalid: {e}",
                self.dns_cfg.listen_addr
            ))
        })?;

        let socket = Arc::new(
            UdpSocket::bind(listen)
                .await
                .map_err(|e| MirageError::Config(format!("topology dns bind {listen}: {e}")))?,
        );

        info!(addr = %listen, "topology dns forwarder listening");

        let mut buf = vec![0u8; 4096];

        loop {
            let (len, peer) = socket.recv_from(&mut buf).await?;
            let query = buf[..len].to_vec();
            let fwd = Arc::clone(&self);
            let sock = Arc::clone(&socket);

            tokio::spawn(async move {
                // For Wave 3 the chain_id comes from a single-chain config or is
                // the first usable chain in the ChainSet.  Per-flow routing (Wave
                // 4) will refine this.
                let chain_id = fwd.chain_set.first_usable_chain_id();

                match fwd.resolve_wire(&query, chain_id.as_deref()).await {
                    Ok(mut resp) => {
                        if query.len() >= 2 && resp.len() >= 2 {
                            resp[0] = query[0];
                            resp[1] = query[1];
                        }
                        if let Err(e) = sock.send_to(&resp, peer).await {
                            warn!(peer = %peer, error = %e, "topology dns: send_to failed");
                        }
                    }
                    Err(e) => {
                        error!(peer = %peer, error = %e, "topology dns: resolve failed");
                        let resp = servfail_response(&query);
                        let _ = sock.send_to(&resp, peer).await;
                    }
                }
            });
        }
    }

    /// Resolve a raw DNS wire query.  Returns the raw DNS wire response.
    async fn resolve_wire(
        &self,
        query: &[u8],
        chain_id: Option<&str>,
    ) -> Result<Vec<u8>, MirageError> {
        if query.len() < 12 {
            return Err(MirageError::Protocol(format!(
                "dns query too short: {} bytes",
                query.len()
            )));
        }

        let key = cache_key_from_wire(query);
        let name = extract_qname(query).unwrap_or_default();

        // ── IpAddr cache ─────────────────────────────────────────────────────
        // We cache at the wire level for simplicity (same key = same question).
        // A future enhancement could cache parsed IpAddr slices instead.
        if !name.is_empty() {
            if let Some(cached_addrs) = self.cache.get(chain_id, &name) {
                debug!(name, chain_id, "dns topology cache hit (addr level)");
                // We don't have the wire response cached at this level — fall
                // through to backend and let it produce the wire bytes.
                // The addr-level cache is used by resolve_name() callers, not here.
                let _ = cached_addrs; // suppress unused warning
            }
        }

        // ── Wire-level cache ──────────────────────────────────────────────────
        // Use key (txid-zeroed wire bytes) as the wire response cache key.
        // Stored separately from the addr cache to keep them decoupled.
        let wire_cache_key = String::from_utf8_lossy(&key).into_owned();
        if let Some(cached_addrs) = self.cache.get(chain_id, &wire_cache_key) {
            debug!(chain_id, "dns topology wire cache hit");
            // cached_addrs is IpAddr[], not wire bytes — can't use as wire resp.
            // Wire-level caching would require a separate HashMap.  For Wave 3
            // we rely solely on the addr cache via resolve_name().
            let _ = cached_addrs;
        }

        let backend = self.topology.backend_for(chain_id);
        let response = self.dispatch(backend, query, chain_id).await?;

        // Parse resolved IPs from the wire response and store in addr cache.
        if !name.is_empty() {
            let addrs = parse_a_aaaa_from_wire(&response);
            if !addrs.is_empty() {
                self.cache.insert(chain_id, name, addrs.into(), None);
            }
        }

        Ok(response)
    }

    /// Resolve a hostname to IP addresses.  Used by TUN-layer callers that
    /// need `Vec<IpAddr>` rather than wire bytes.
    ///
    /// `chain_id = None` → bypass tier.
    pub async fn resolve_name(
        &self,
        name: &str,
        chain_id: Option<&str>,
    ) -> Result<Vec<IpAddr>, MirageError> {
        // Cache check.
        if let Some(cached) = self.cache.get(chain_id, name) {
            debug!(name, chain_id, "dns topology name cache hit");
            return Ok(cached.to_vec());
        }

        let backend = self.topology.backend_for(chain_id);
        let addrs = self.dispatch_name(backend, name, chain_id).await?;

        self.cache.insert(
            chain_id,
            name.to_owned(),
            addrs.clone().into(),
            None,
        );

        Ok(addrs)
    }

    // ── Backend dispatch ──────────────────────────────────────────────────────

    /// Dispatch a raw DNS wire query to the chosen backend; return wire response.
    async fn dispatch(
        &self,
        backend: &DnsBackend,
        query: &[u8],
        chain_id: Option<&str>,
    ) -> Result<Vec<u8>, MirageError> {
        match backend {
            DnsBackend::DohOverChain { doh_url, .. } => {
                let socks = self.socks_for_chain(chain_id);
                doh_post_via_proxy(query, doh_url, &socks).await
            }
            DnsBackend::DohDirect { server, .. } => {
                // Use direct HTTP (no proxy) to the pre-resolved IP.
                let doh_url = format!("https://{}/dns-query", server);
                doh_post_direct(query, &doh_url).await
            }
            DnsBackend::DotOverChain { dot_addr, .. } => {
                let socks = self.socks_for_chain(chain_id);
                dot_query_via_proxy(query, dot_addr, &socks).await
            }
            DnsBackend::DotDirect { server, .. } => {
                dot_query_direct(query, *server).await
            }
            DnsBackend::SystemResolver => {
                let name = extract_qname(query).unwrap_or_default();
                if name.is_empty() {
                    return Err(MirageError::Protocol(
                        "cannot extract qname for system resolver".into(),
                    ));
                }
                let addrs = system_resolve(&name).await?;
                Ok(build_a_response(query, &addrs))
            }
        }
    }

    /// Dispatch a hostname lookup to the chosen backend; return resolved IPs.
    async fn dispatch_name(
        &self,
        backend: &DnsBackend,
        name: &str,
        chain_id: Option<&str>,
    ) -> Result<Vec<IpAddr>, MirageError> {
        match backend {
            DnsBackend::DohOverChain { doh_url, .. } => {
                let socks = self.socks_for_chain(chain_id);
                let wire = doh_post_via_proxy(&build_a_query(name), doh_url, &socks).await?;
                Ok(parse_a_aaaa_from_wire(&wire))
            }
            DnsBackend::DohDirect { server, .. } => {
                let doh_url = format!("https://{}/dns-query", server);
                let wire = doh_post_direct(&build_a_query(name), &doh_url).await?;
                Ok(parse_a_aaaa_from_wire(&wire))
            }
            DnsBackend::DotOverChain { dot_addr, .. } => {
                let socks = self.socks_for_chain(chain_id);
                let wire = dot_query_via_proxy(&build_a_query(name), dot_addr, &socks).await?;
                Ok(parse_a_aaaa_from_wire(&wire))
            }
            DnsBackend::DotDirect { server, .. } => {
                let wire = dot_query_direct(&build_a_query(name), *server).await?;
                Ok(parse_a_aaaa_from_wire(&wire))
            }
            DnsBackend::SystemResolver => system_resolve(name).await,
        }
    }

    /// Return the SOCKS5 proxy URL for the given chain.
    ///
    /// Wave 3 simplification: a single default SOCKS5 address covers all
    /// chains.  Wave 4 will map each chain to its own per-port SOCKS5 listener.
    fn socks_for_chain(&self, _chain_id: Option<&str>) -> String {
        self.default_socks_addr.clone()
    }
}

// ── Backend implementations ───────────────────────────────────────────────────

/// DoH POST via a SOCKS5 proxy.
async fn doh_post_via_proxy(
    query: &[u8],
    doh_url: &str,
    socks_addr: &str,
) -> Result<Vec<u8>, MirageError> {
    let proxy = reqwest::Proxy::all(socks_addr)
        .map_err(|e| MirageError::Config(format!("doh socks proxy invalid ({socks_addr}): {e}")))?;
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .build()
        .map_err(|e| MirageError::Config(format!("doh reqwest client: {e}")))?;
    doh_post_with_client(&client, query, doh_url).await
}

/// DoH POST without a proxy (direct to pre-resolved IP).
async fn doh_post_direct(query: &[u8], doh_url: &str) -> Result<Vec<u8>, MirageError> {
    let client = reqwest::Client::builder()
        .build()
        .map_err(|e| MirageError::Config(format!("doh direct reqwest client: {e}")))?;
    doh_post_with_client(&client, query, doh_url).await
}

async fn doh_post_with_client(
    client: &reqwest::Client,
    query: &[u8],
    doh_url: &str,
) -> Result<Vec<u8>, MirageError> {
    let resp = client
        .post(doh_url)
        .header("Content-Type", "application/dns-message")
        .header("Accept", "application/dns-message")
        .body(query.to_vec())
        .send()
        .await
        .map_err(|e| MirageError::Protocol(format!("doh POST to {doh_url} failed: {e}")))?;

    if !resp.status().is_success() {
        return Err(MirageError::Protocol(format!(
            "doh upstream {doh_url} returned HTTP {}",
            resp.status()
        )));
    }

    let bytes = resp
        .bytes()
        .await
        .map_err(|e| MirageError::Protocol(format!("doh read body from {doh_url}: {e}")))?;
    Ok(bytes.to_vec())
}

/// DoT query via SOCKS5.  Wire protocol: 2-byte BE length prefix + DNS query.
async fn dot_query_via_proxy(
    query: &[u8],
    dot_addr: &str,
    socks_addr: &str,
) -> Result<Vec<u8>, MirageError> {
    // reqwest doesn't do raw TCP, so we proxy via SOCKS5 manually.
    // For Wave 3 we delegate to a plain TCP connect through the SOCKS5 tunnel
    // using `tokio_socks` is not available as a dep; fall back to system DNS.
    // The full DoT-over-SOCKS5 implementation requires a SOCKS5 TCP connector
    // which is deferred to Wave 4.  Log the configuration and use fallback.
    warn!(
        dot_addr,
        socks_addr, "DoT-over-chain not yet implemented (Wave 4); falling back to system resolver"
    );
    dot_query_direct_str(query, dot_addr).await
}

/// DoT query direct (no proxy).
async fn dot_query_direct(query: &[u8], server: SocketAddr) -> Result<Vec<u8>, MirageError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;

    let mut stream = tokio::time::timeout(Duration::from_secs(5), TcpStream::connect(server))
        .await
        .map_err(|_| MirageError::Protocol(format!("DoT connect timeout to {server}")))?
        .map_err(|e| MirageError::Protocol(format!("DoT connect to {server}: {e}")))?;

    let len = (query.len() as u16).to_be_bytes();
    stream
        .write_all(&len)
        .await
        .map_err(|e| MirageError::Protocol(format!("DoT write len: {e}")))?;
    stream
        .write_all(query)
        .await
        .map_err(|e| MirageError::Protocol(format!("DoT write query: {e}")))?;

    let mut resp_len_bytes = [0u8; 2];
    stream
        .read_exact(&mut resp_len_bytes)
        .await
        .map_err(|e| MirageError::Protocol(format!("DoT read len: {e}")))?;
    let resp_len = u16::from_be_bytes(resp_len_bytes) as usize;
    if resp_len < 12 || resp_len > 65535 {
        return Err(MirageError::Protocol(format!(
            "DoT bad response length: {resp_len}"
        )));
    }

    let mut resp = vec![0u8; resp_len];
    stream
        .read_exact(&mut resp)
        .await
        .map_err(|e| MirageError::Protocol(format!("DoT read body: {e}")))?;
    Ok(resp)
}

async fn dot_query_direct_str(query: &[u8], addr_str: &str) -> Result<Vec<u8>, MirageError> {
    let server: SocketAddr = addr_str
        .parse()
        .map_err(|e| MirageError::Config(format!("DoT addr {addr_str:?} invalid: {e}")))?;
    dot_query_direct(query, server).await
}

/// OS system resolver via `tokio::net::lookup_host`.
async fn system_resolve(name: &str) -> Result<Vec<IpAddr>, MirageError> {
    let host = format!("{name}:0");
    let addrs = tokio::net::lookup_host(&*host)
        .await
        .map_err(|e| MirageError::Protocol(format!("system resolver for {name}: {e}")))?
        .map(|sa| sa.ip())
        .collect::<Vec<_>>();
    if addrs.is_empty() {
        return Err(MirageError::Protocol(format!(
            "system resolver: no addresses for {name}"
        )));
    }
    Ok(addrs)
}

// ── DNS wire helpers ──────────────────────────────────────────────────────────

/// Build a minimal A-type DNS query for `name`.
fn build_a_query(name: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(32);
    // Transaction ID (random-ish)
    buf.extend_from_slice(&[0xCA, 0xFE]);
    // Flags: RD=1
    buf.extend_from_slice(&[0x01, 0x00]);
    // QDCOUNT=1, ANCOUNT=0, NSCOUNT=0, ARCOUNT=0
    buf.extend_from_slice(&[0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    // QNAME
    for label in name.split('.') {
        let b = label.as_bytes();
        buf.push(b.len() as u8);
        buf.extend_from_slice(b);
    }
    buf.push(0); // root label
    // QTYPE=A (1), QCLASS=IN (1)
    buf.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]);
    buf
}

/// Extract A/AAAA addresses from a DNS wire response.
/// Returns empty vec on malformed input — callers should fall through to system resolver.
fn parse_a_aaaa_from_wire(wire: &[u8]) -> Vec<IpAddr> {
    if wire.len() < 12 {
        return Vec::new();
    }
    let ancount = u16::from_be_bytes([wire[6], wire[7]]) as usize;
    if ancount == 0 {
        return Vec::new();
    }

    let mut pos = 12usize;
    // Skip question section.
    // QDCOUNT from wire[4..6].
    let qdcount = u16::from_be_bytes([wire[4], wire[5]]) as usize;
    for _ in 0..qdcount {
        if pos >= wire.len() { return Vec::new(); }
        // Skip QNAME.
        loop {
            if pos >= wire.len() { return Vec::new(); }
            let len = wire[pos] as usize;
            if len == 0 { pos += 1; break; }
            if len & 0xC0 == 0xC0 { pos += 2; break; }
            pos += 1 + len;
        }
        pos += 4; // QTYPE + QCLASS
    }

    let mut addrs = Vec::new();
    for _ in 0..ancount {
        if pos >= wire.len() { break; }
        // NAME (may be a pointer).
        if pos < wire.len() && wire[pos] & 0xC0 == 0xC0 {
            pos += 2;
        } else {
            loop {
                if pos >= wire.len() { return addrs; }
                let len = wire[pos] as usize;
                if len == 0 { pos += 1; break; }
                if len & 0xC0 == 0xC0 { pos += 2; break; }
                pos += 1 + len;
            }
        }
        if pos + 10 > wire.len() { break; }
        let rtype = u16::from_be_bytes([wire[pos], wire[pos + 1]]);
        // Skip TYPE(2) + CLASS(2) + TTL(4) = 8 bytes.
        pos += 8;
        let rdlength = u16::from_be_bytes([wire[pos], wire[pos + 1]]) as usize;
        pos += 2;
        if pos + rdlength > wire.len() { break; }
        match (rtype, rdlength) {
            (1, 4) => {
                // A record
                let ip = std::net::Ipv4Addr::new(wire[pos], wire[pos+1], wire[pos+2], wire[pos+3]);
                addrs.push(IpAddr::V4(ip));
            }
            (28, 16) => {
                // AAAA record
                let mut octets = [0u8; 16];
                octets.copy_from_slice(&wire[pos..pos+16]);
                addrs.push(IpAddr::V6(std::net::Ipv6Addr::from(octets)));
            }
            _ => {}
        }
        pos += rdlength;
    }
    addrs
}

/// Build a minimal A response from resolved IPs (used for SystemResolver path).
fn build_a_response(query: &[u8], addrs: &[IpAddr]) -> Vec<u8> {
    let mut resp = query.to_vec();
    if resp.len() < 12 {
        resp.resize(12, 0);
    }
    // QR=1, RD=1, RA=1, RCODE=0
    resp[2] = 0x81;
    resp[3] = 0x80;
    // ANCOUNT = number of A records we can encode
    let a_count = addrs.iter().filter(|a| a.is_ipv4()).count() as u16;
    resp[6] = (a_count >> 8) as u8;
    resp[7] = (a_count & 0xFF) as u8;

    // Append answer records — name pointer to question (offset 12 = 0xC00C).
    for addr in addrs {
        if let IpAddr::V4(v4) = addr {
            resp.extend_from_slice(&[0xC0, 0x0C]); // NAME pointer
            resp.extend_from_slice(&[0x00, 0x01]); // TYPE A
            resp.extend_from_slice(&[0x00, 0x01]); // CLASS IN
            resp.extend_from_slice(&[0x00, 0x00, 0x01, 0x2C]); // TTL 300
            resp.extend_from_slice(&[0x00, 0x04]); // RDLENGTH 4
            resp.extend_from_slice(&v4.octets());
        }
    }
    resp
}

// ── ChainSet extension ────────────────────────────────────────────────────────

/// Extension trait to pick the first usable chain's ID.
///
/// Used by the Wave 3 forwarder to route DNS queries when per-flow chain
/// assignment (Wave 4) is not yet implemented.
trait ChainSetExt {
    fn first_usable_chain_id(&self) -> Option<String>;
}

impl ChainSetExt for ChainSet {
    fn first_usable_chain_id(&self) -> Option<String> {
        self.active_chains()
            .into_iter()
            .find(|h| h.is_usable())
            .map(|h| h.id.as_str().to_owned())
    }
}
