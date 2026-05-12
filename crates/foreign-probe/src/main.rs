//! foreign-probe — minimal SOCKS5 inbound that speaks one foreign wire
//! (Shadowsocks-AEAD-2017 or Trojan-TLS) toward a Mirage standalone
//! terminator.
//!
//! Used to bisect "v2rayTun on phone shows connected but no traffic":
//! if `curl --socks5-hostname` through `foreign-probe` from the Mac (same
//! ISP environment) works, the wire is correct and the bug lives in
//! the mobile client. If it fails the same way as the phone — the bug
//! is in our impl.
//!
//! Usage:
//!   foreign-probe --listen 127.0.0.1:1080 \
//!                 --wire ss --server 91.218.245.247:8388 \
//!                 --ss-method chacha20-ietf-poly1305 \
//!                 --ss-password MirageSS-AEAD-2026
//!
//!   foreign-probe --listen 127.0.0.1:1080 \
//!                 --wire trojan --server 91.218.245.247:8443 \
//!                 --trojan-password MirageTrojan-2026 \
//!                 --trojan-sni www.cloudflare.com

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, ValueEnum};
use rand::RngCore;
use std::{net::Ipv6Addr, sync::Arc};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::{rustls, TlsConnector};
use tracing::{debug, info, warn};

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Wire {
    Ss,
    Trojan,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum SsMethod {
    #[value(name = "aes-128-gcm")]      Aes128Gcm,
    #[value(name = "aes-256-gcm")]      Aes256Gcm,
    #[value(name = "chacha20-ietf-poly1305")] Chacha,
}

#[derive(Parser, Debug)]
#[command(version, about = "SOCKS5 \u{2192} foreign-wire (SS / Trojan) probe")]
struct Args {
    /// SOCKS5 listen address (e.g. 127.0.0.1:1080).
    #[arg(long, default_value = "127.0.0.1:1080")]
    listen: String,

    /// Upstream wire type.
    #[arg(long)]
    wire: Wire,

    /// host:port of the foreign-wire server.
    #[arg(long)]
    server: String,

    // ── SS-specific ────────────────────────────────────────────────
    #[arg(long, default_value = "chacha20-ietf-poly1305")]
    ss_method: SsMethod,
    #[arg(long, default_value = "")]
    ss_password: String,

    // ── Trojan-specific ────────────────────────────────────────────
    #[arg(long, default_value = "")]
    trojan_password: String,
    #[arg(long, default_value = "www.cloudflare.com")]
    trojan_sni: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args = Arc::new(Args::parse());
    let listener = TcpListener::bind(&args.listen).await
        .with_context(|| format!("bind {}", args.listen))?;
    info!(
        listen = %args.listen,
        wire = ?args.wire,
        server = %args.server,
        "foreign-probe ready"
    );
    loop {
        let (sock, peer) = listener.accept().await.context("accept")?;
        let args = args.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(args, sock).await {
                warn!(%peer, "session error: {e:#}");
            }
        });
    }
}

async fn handle_client(args: Arc<Args>, mut client: TcpStream) -> Result<()> {
    let target = read_socks5_request(&mut client).await?;
    debug!(target = %target_to_str(&target), "socks5 target");
    match args.wire {
        Wire::Ss => proxy_via_ss(&args, client, target).await,
        Wire::Trojan => proxy_via_trojan(&args, client, target).await,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SOCKS5 inbound (no auth, CONNECT only — sufficient for `curl --socks5-hostname`)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum Target {
    Ipv4([u8; 4], u16),
    Domain(String, u16),
    Ipv6([u8; 16], u16),
}

fn target_to_str(t: &Target) -> String {
    match t {
        Target::Ipv4(ip, p) => format!("{}.{}.{}.{}:{p}", ip[0], ip[1], ip[2], ip[3]),
        Target::Domain(host, p) => format!("{host}:{p}"),
        Target::Ipv6(ip, p) => format!("[{}]:{p}", Ipv6Addr::from(*ip)),
    }
}

async fn read_socks5_request(s: &mut TcpStream) -> Result<Target> {
    let mut ver = [0u8; 2];
    s.read_exact(&mut ver).await?;
    if ver[0] != 0x05 {
        bail!("not SOCKS5");
    }
    let n_methods = ver[1] as usize;
    let mut methods = vec![0u8; n_methods];
    s.read_exact(&mut methods).await?;
    // reply: VER=5, no auth
    s.write_all(&[0x05, 0x00]).await?;
    let mut req = [0u8; 4];
    s.read_exact(&mut req).await?;
    if req[0] != 0x05 || req[1] != 0x01 {
        bail!("only SOCKS5 CONNECT supported");
    }
    let target = match req[3] {
        0x01 => {
            let mut ip = [0u8; 4];
            s.read_exact(&mut ip).await?;
            let mut port = [0u8; 2];
            s.read_exact(&mut port).await?;
            Target::Ipv4(ip, u16::from_be_bytes(port))
        }
        0x03 => {
            let mut len = [0u8; 1];
            s.read_exact(&mut len).await?;
            let mut host = vec![0u8; len[0] as usize];
            s.read_exact(&mut host).await?;
            let mut port = [0u8; 2];
            s.read_exact(&mut port).await?;
            Target::Domain(
                String::from_utf8(host).map_err(|e| anyhow!("bad domain: {e}"))?,
                u16::from_be_bytes(port),
            )
        }
        0x04 => {
            let mut ip = [0u8; 16];
            s.read_exact(&mut ip).await?;
            let mut port = [0u8; 2];
            s.read_exact(&mut port).await?;
            Target::Ipv6(ip, u16::from_be_bytes(port))
        }
        other => bail!("unsupported SOCKS5 ATYP {other}"),
    };
    // success reply (BND.ADDR=0.0.0.0:0)
    s.write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await?;
    Ok(target)
}

fn target_to_socks_bytes(t: &Target) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 1 + 256 + 2);
    match t {
        Target::Ipv4(ip, p) => {
            out.push(0x01);
            out.extend_from_slice(ip);
            out.extend_from_slice(&p.to_be_bytes());
        }
        Target::Domain(host, p) => {
            out.push(0x03);
            out.push(host.len() as u8);
            out.extend_from_slice(host.as_bytes());
            out.extend_from_slice(&p.to_be_bytes());
        }
        Target::Ipv6(ip, p) => {
            out.push(0x04);
            out.extend_from_slice(ip);
            out.extend_from_slice(&p.to_be_bytes());
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Shadowsocks-AEAD-2017 client
// ─────────────────────────────────────────────────────────────────────────────

const SS_INFO: &[u8] = b"ss-subkey";
const SS_TAG: usize = 16;
const SS_NONCE_LEN: usize = 12;
const SS_MAX_CHUNK: usize = 0x3fff;

impl SsMethod {
    fn key_len(self) -> usize {
        match self {
            SsMethod::Aes128Gcm => 16,
            SsMethod::Aes256Gcm | SsMethod::Chacha => 32,
        }
    }
    fn salt_len(self) -> usize { self.key_len() }
}

fn evp_bytes_to_key(password: &[u8], key_len: usize) -> Vec<u8> {
    use md5::{Digest, Md5};
    let mut out = Vec::with_capacity(key_len);
    let mut prev: Vec<u8> = Vec::new();
    while out.len() < key_len {
        let mut h = Md5::new();
        if !prev.is_empty() { h.update(&prev); }
        h.update(password);
        prev = h.finalize().to_vec();
        out.extend_from_slice(&prev);
    }
    out.truncate(key_len);
    out
}

fn ss_derive_subkey(master: &[u8], salt: &[u8], key_len: usize) -> Result<Vec<u8>> {
    use hkdf::Hkdf;
    use sha1::Sha1;
    let hk = Hkdf::<Sha1>::new(Some(salt), master);
    let mut out = vec![0u8; key_len];
    hk.expand(SS_INFO, &mut out).map_err(|e| anyhow!("hkdf: {e}"))?;
    Ok(out)
}

enum SsCipher {
    Aes128(aes_gcm::Aes128Gcm),
    Aes256(aes_gcm::Aes256Gcm),
    Chacha(chacha20poly1305::ChaCha20Poly1305),
}

impl SsCipher {
    fn new(method: SsMethod, key: &[u8]) -> Result<Self> {
        use aes_gcm::aead::KeyInit;
        use chacha20poly1305::KeyInit as _;
        Ok(match method {
            SsMethod::Aes128Gcm => SsCipher::Aes128(
                aes_gcm::Aes128Gcm::new_from_slice(key).map_err(|e| anyhow!("aes128: {e}"))?,
            ),
            SsMethod::Aes256Gcm => SsCipher::Aes256(
                aes_gcm::Aes256Gcm::new_from_slice(key).map_err(|e| anyhow!("aes256: {e}"))?,
            ),
            SsMethod::Chacha => SsCipher::Chacha(
                chacha20poly1305::ChaCha20Poly1305::new_from_slice(key)
                    .map_err(|e| anyhow!("chacha: {e}"))?,
            ),
        })
    }

    fn encrypt(&self, nonce: &[u8; SS_NONCE_LEN], plain: &[u8]) -> Result<Vec<u8>> {
        use aes_gcm::aead::Aead;
        let nonce = aes_gcm::Nonce::from_slice(nonce);
        match self {
            SsCipher::Aes128(c) => c.encrypt(nonce, plain).map_err(|e| anyhow!("encrypt: {e}")),
            SsCipher::Aes256(c) => c.encrypt(nonce, plain).map_err(|e| anyhow!("encrypt: {e}")),
            SsCipher::Chacha(c) => {
                use chacha20poly1305::aead::Aead;
                c.encrypt(
                    chacha20poly1305::Nonce::from_slice(nonce.as_slice()),
                    plain,
                )
                .map_err(|e| anyhow!("encrypt: {e}"))
            }
        }
    }

    fn decrypt(&self, nonce: &[u8; SS_NONCE_LEN], ct: &[u8]) -> Result<Vec<u8>> {
        use aes_gcm::aead::Aead;
        let nonce = aes_gcm::Nonce::from_slice(nonce);
        match self {
            SsCipher::Aes128(c) => c.decrypt(nonce, ct).map_err(|e| anyhow!("decrypt: {e}")),
            SsCipher::Aes256(c) => c.decrypt(nonce, ct).map_err(|e| anyhow!("decrypt: {e}")),
            SsCipher::Chacha(c) => {
                use chacha20poly1305::aead::Aead;
                c.decrypt(
                    chacha20poly1305::Nonce::from_slice(nonce.as_slice()),
                    ct,
                )
                .map_err(|e| anyhow!("decrypt: {e}"))
            }
        }
    }
}

fn nonce_from_counter(counter: u64) -> [u8; SS_NONCE_LEN] {
    let mut n = [0u8; SS_NONCE_LEN];
    n[..8].copy_from_slice(&counter.to_le_bytes());
    n
}

struct SsReadHalf {
    rd: ReadHalf<TcpStream>,
    cipher: SsCipher,
    counter: u64,
    overflow: Vec<u8>,
}

struct SsWriteHalf {
    wr: WriteHalf<TcpStream>,
    cipher: SsCipher,
    counter: u64,
}

impl SsReadHalf {
    async fn read_chunk(&mut self) -> Result<Option<Vec<u8>>> {
        let mut enc_len = [0u8; 2 + SS_TAG];
        match self.rd.read_exact(&mut enc_len).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => bail!("ss read len: {e}"),
        }
        let n = nonce_from_counter(self.counter);
        self.counter += 1;
        let len_plain = self.cipher.decrypt(&n, &enc_len)?;
        let payload_len = u16::from_be_bytes([len_plain[0], len_plain[1]]) as usize;
        if payload_len > SS_MAX_CHUNK {
            bail!("ss payload chunk too large: {payload_len}");
        }
        let mut enc_payload = vec![0u8; payload_len + SS_TAG];
        self.rd.read_exact(&mut enc_payload).await?;
        let n = nonce_from_counter(self.counter);
        self.counter += 1;
        Ok(Some(self.cipher.decrypt(&n, &enc_payload)?))
    }
}

impl SsWriteHalf {
    async fn write_chunk(&mut self, chunk: &[u8]) -> Result<()> {
        for sub in chunk.chunks(SS_MAX_CHUNK) {
            let len_be = (sub.len() as u16).to_be_bytes();
            let n = nonce_from_counter(self.counter); self.counter += 1;
            let enc_len = self.cipher.encrypt(&n, &len_be)?;
            let n = nonce_from_counter(self.counter); self.counter += 1;
            let enc_payload = self.cipher.encrypt(&n, sub)?;
            self.wr.write_all(&enc_len).await?;
            self.wr.write_all(&enc_payload).await?;
        }
        Ok(())
    }
}

async fn proxy_via_ss(args: &Args, mut client: TcpStream, target: Target) -> Result<()> {
    let master = evp_bytes_to_key(args.ss_password.as_bytes(), args.ss_method.key_len());
    let mut upstream = TcpStream::connect(&args.server).await
        .with_context(|| format!("ss connect {}", args.server))?;
    // Client salt
    let mut client_salt = vec![0u8; args.ss_method.salt_len()];
    rand::rngs::OsRng.fill_bytes(&mut client_salt);
    let write_subkey = ss_derive_subkey(&master, &client_salt, args.ss_method.key_len())?;
    let write_cipher = SsCipher::new(args.ss_method, &write_subkey)?;
    upstream.write_all(&client_salt).await?;
    // Server salt
    let mut server_salt = vec![0u8; args.ss_method.salt_len()];
    upstream.read_exact(&mut server_salt).await?;
    let read_subkey = ss_derive_subkey(&master, &server_salt, args.ss_method.key_len())?;
    let read_cipher = SsCipher::new(args.ss_method, &read_subkey)?;

    let (up_rd, up_wr) = tokio::io::split(upstream);
    let mut up_r = SsReadHalf {
        rd: up_rd, cipher: read_cipher, counter: 0, overflow: Vec::new(),
    };
    let mut up_w = SsWriteHalf {
        wr: up_wr, cipher: write_cipher, counter: 0,
    };
    // First payload = target header (SOCKS-style)
    let hdr = target_to_socks_bytes(&target);
    up_w.write_chunk(&hdr).await?;

    bidir_ss(&mut client, &mut up_r, &mut up_w).await
}

async fn bidir_ss(
    client: &mut TcpStream,
    up_r: &mut SsReadHalf,
    up_w: &mut SsWriteHalf,
) -> Result<()> {
    let (mut c_rd, mut c_wr) = client.split();
    let client_to_up = async {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = c_rd.read(&mut buf).await?;
            if n == 0 {
                let _ = up_w.wr.shutdown().await;
                return Ok::<(), anyhow::Error>(());
            }
            up_w.write_chunk(&buf[..n]).await?;
        }
    };
    let up_to_client = async {
        loop {
            match up_r.read_chunk().await? {
                Some(chunk) => c_wr.write_all(&chunk).await?,
                None => { let _ = c_wr.shutdown().await; return Ok::<(), anyhow::Error>(()); }
            }
        }
    };
    tokio::select! {
        r = client_to_up => r?,
        r = up_to_client => r?,
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Trojan-TLS client
// ─────────────────────────────────────────────────────────────────────────────

async fn proxy_via_trojan(args: &Args, mut client: TcpStream, target: Target) -> Result<()> {
    let connector = trojan_connector()?;
    let tcp = TcpStream::connect(&args.server).await
        .with_context(|| format!("trojan connect {}", args.server))?;
    let sni = rustls::pki_types::ServerName::try_from(args.trojan_sni.clone())
        .map_err(|e| anyhow!("bad sni: {e}"))?;
    let mut tls = connector.connect(sni, tcp).await
        .map_err(|e| anyhow!("trojan tls handshake: {e}"))?;

    // password hash = lowercase hex of SHA224(password)
    use sha2::{Digest, Sha224};
    let pass_hash = Sha224::digest(args.trojan_password.as_bytes());
    let mut hex_hash = [0u8; 56];
    hex::encode_to_slice(pass_hash, &mut hex_hash)?;
    // Build header: hash[56] || CRLF || cmd(=0x01 TCP) || atype+addr+port || CRLF
    let mut hdr = Vec::with_capacity(64 + 1 + 256);
    hdr.extend_from_slice(&hex_hash);
    hdr.extend_from_slice(b"\r\n");
    hdr.push(0x01);
    hdr.extend_from_slice(&target_to_socks_bytes(&target));
    hdr.extend_from_slice(b"\r\n");
    tls.write_all(&hdr).await?;

    let _ = tokio::io::copy_bidirectional(&mut client, &mut tls).await?;
    Ok(())
}

fn trojan_connector() -> Result<TlsConnector> {
    let mut cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(TlsConnector::from(Arc::new(cfg)))
}

#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::ED25519,
        ]
    }
}
