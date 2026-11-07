# mirage2app

m1r4g3 client: CLI + client-core library. Extracted from mirage2web.

## L0 stack (client side)

```
        ┌──────────────────────── LM (mirage2monitor) ────────────────────────┐
        │  9-LM full peer-clique, HTTPS+WSS :19443, peer_lm_urls in config    │
        └──────────────┬─────────────────────────────────────────┬─────────────┘
                       │ WS subscribe                            │ HTTP poll
                       │ + DebugTunnelOpen / DebugTunnelClose    │ (node sentry)
                       ▼                                         ▼
        ┌────────────────────────┐                  ┌────────────────────────┐
        │ mirage-l0-sentinel     │  spawn / kill →  │ mirage-debug-shell     │
        │ (sentry, always-on)    │  reverse-SSH     │ (probe, on-demand)     │
        └─────────┬──────────────┘                  └────────────────────────┘
                  │ spawn                                        ▲
                  ▼                                              │ same binary
        ┌────────────────────────┐                  ┌────────────────────────┐
        │ mirage-client-cli      │  POST            │ mirage2monitor-probe   │
        │ (SOCKS5/HTTP, pools)   │  /chains/update  │ (sentry on each VPS)   │
        └─────────┬──────────────┘─────────►        └────────────────────────┘
                  ▼
        ┌────────────────────────┐
        │ mirage-tun-windows     │
        │ (Wintun, NAT, kill-sw) │
        └────────────────────────┘
```

**Terminology** (May 2026): *sentry* = the always-on monitoring agent (client: `mirage-l0-sentinel`; node: `mirage2monitor-probe`). *probe* = the on-demand reverse-SSH debug worker (`mirage-debug-shell`, deployed as `/usr/local/bin/mirage-probe`) spawned by a sentry only while an operator's debug tunnel is open. See `mirage2app/CLAUDE.md` for the full sentinel.toml field list and `peer_lm_urls` bootstrap order.

## Building

```bash
cd mirage2app && cargo build --release -p mirage-client-cli
```

## Running

```bash
mirage-client-cli.exe --config ../mirage2tun/client-config.json
```

## Testing chains

```bash
curl -x socks5h://127.0.0.1:19480 http://ifconfig.me   # default (byte-cipher-classic)
curl -x socks5h://127.0.0.1:19482 http://ifconfig.me   # classic-direct
curl -x socks5h://127.0.0.1:19483 http://ifconfig.me   # crypto-relay-classic
curl -x socks5h://127.0.0.1:19484 http://ifconfig.me   # three-hop-onion
curl -x socks5h://127.0.0.1:19486 http://ifconfig.me   # three-hop-telescopic
curl -x http://127.0.0.1:19481 http://ifconfig.me       # HTTP CONNECT proxy
```

Classic chains (19480, 19482, 19483) return `203.0.113.17` (L2-CH-TEST IPv4).
Onion chains (19484, 19486) return `2602:fa59:11:616::1` (L2-CH-TEST IPv6, different exit path).

## Port map

| Port | Chain | Protocol |
|------|-------|----------|
| 19480 | byte-cipher-classic (default) | MirageTls |
| 19481 | HTTP CONNECT proxy | — |
| 19482 | classic-direct | Mirage (transparent L1) |
| 19483 | crypto-relay-classic | Mirage (crypto L1) |
| 19484 | three-hop-onion | Mirage (3-hop ephemeral) |
| 19486 | three-hop-telescopic | Mirage (3-hop Tor-style) |

## Config

Client config: `../mirage2tun/client-config.json`

Per-chain SOCKS5 port set via `chains[].socks5_bind`. Default chain gets `--bind` (19480).
