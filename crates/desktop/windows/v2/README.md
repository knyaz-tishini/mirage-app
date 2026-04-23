# mirage2light-app

Desktop GUI for mirage-client. Runs without admin — spawns mirage-client.exe elevated via UAC.

## APIs

### Agent API — port 19450, no auth

| Method | Path | Description |
|--------|------|-------------|
| GET | /status | App state + mirage-client stats |
| GET | /logs | All log lines (timestamped) |
| POST | /config | Set ClientConfig JSON |
| POST | /connect | Start mirage-client |
| POST | /disconnect | Stop mirage-client |

#### GET /status
```json
{
  "state": "running",
  "stats": {
    "active_chain_id": "classic-direct",
    "mode_socks5": true,
    "mode_http": false,
    "mode_tun": true,
    "pool_size": 3,
    "pool_idle": 2,
    "pool_leased": 1,
    "pool_reconnecting": 0,
    "cover_traffic_enabled": false,
    "config_version": 1
  }
}
```
`state` values: `"stopped"` `"starting"` `"running"` `"error"`

#### GET /logs
```json
["26:45 [OK] starting mirage-client...", "26:51 [OK] connected"]
```

#### POST /config
Body: full `ClientConfig` JSON. Overwrites current config. Takes effect on next connect.

#### POST /connect
No body. Starts mirage-client with current config. Triggers UAC prompt.

#### POST /disconnect
No body. Kills mirage-client process, restores network.

---

### Management API — port 4498, Bearer auth

Proxied through mirage-client subprocess. Key auto-generated at connect time.

| Method | Path | Description |
|--------|------|-------------|
| GET | /admin/health | Health check |
| GET | /admin/status | Pool stats (same as `stats` in /status) |

Header: `Authorization: Bearer <key>`

---

## Architecture

```
mirage2light.exe (GUI, no admin)
  ├── Agent API (port 19450, no auth)
  ├── Polls mirage-client management API every 2s
  └── Spawns mirage-client.exe elevated (admin, UAC)
        ├── Management API (port 4498, Bearer auth)
        ├── SOCKS5 proxy (port 1080)
        ├── TUN mode (Wintun + NAT bridge)
        └── WFP kill-switch (dynamic session)
```

---

## Config Loading

Default config loaded from (first found):
1. `../mirage2monitor/mirage-testbed-persistent-0.json`
2. `mirage-testbed-persistent-0.json`

Windows patches applied automatically:
- `tun.tun_name = "mirage0"`
- `tun.tun_addr = "10.85.0.1/24"`
- `dns.listen_addr = "127.0.0.1:5353"`
- `kill_switch = false`

---

## UI Toggles

| Toggle | Options | Config field |
|--------|---------|-------------|
| CHAIN | CLASSIC-DIRECT, BYTE-CIPHER, CRYPTO-RELAY, THREE-HOP-ONION, TELESCOPIC | `chains[].chain_id` |
| CAMOUFLAGE | RAW, JSON-API, GRPC, H2DATA | `chains[].camouflage` |
| SHAPING | NONE, WEB-BROWSE, CLOUD-API, STREAMING | `chains[].shaping` |
| CLIENT MODE | SOCKS5, HTTP-CONNECT, TUN | `tun.enabled`, `http_proxy.enabled` |

---

## Recovery

If network breaks after disconnect (admin PowerShell):

```powershell
route delete 0.0.0.0 mask 0.0.0.0 10.85.0.1
```

WFP filters auto-removed when mirage-client process dies.
