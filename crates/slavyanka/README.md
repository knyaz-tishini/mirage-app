# mirage-slavyanka (MVP, tester1)

Minimal reverse-SSH worker. Opens an outbound SSH session from this host to a
chosen LM and asks the LM to forward a remote port back to this machine's
local OpenSSH server. We then ssh **to the LM** and get a shell **on this
machine**.

**Scope (deliberately narrow):**

- Only for **our own trusted box** (tester1). No signed manifest, no anti-replay,
  no per-target embedded keys, no operator hardware-token verify. See
  `mirage2monitor/docs/admin-layer-concept.md` for the full target design we
  are NOT implementing here.
- One reverse port. One jump host. SOCKS5 hop is optional (so traffic to the
  LM goes through `mirage-client-cli`'s local proxy).

## Operator workflow

### 1. Pre-reqs on tester1 (Win11)

The Windows OpenSSH server must be running so the reverse forward has somewhere
to land. Open *Optional features* and install `OpenSSH Server`, then in an
elevated PowerShell:

```powershell
Set-Service -Name sshd -StartupType Automatic
Start-Service sshd
# verify:
Get-NetTCPConnection -LocalPort 22
```

Add a public key for your operator account to `C:\Users\<you>\.ssh\authorized_keys`
so the eventual `ssh -p 22500 <you>@lm-nl-1` lands.

### 2. Generate the slavyanka identity key

On tester1:

```powershell
ssh-keygen -t ed25519 -f C:\ProgramData\mirage\slavyanka_ed25519 -N ""
```

`-N ""` = no passphrase (this is the trusted-box MVP).

Copy the `.pub` to the LM:

```powershell
type C:\ProgramData\mirage\slavyanka_ed25519.pub
```

On `lm-nl-1.zen-geldmaschine.net` (as the LM operator, via existing
`mash-ssh-config` jump):

```bash
# Create a dedicated user with no shell beyond receiving the reverse tunnel.
sudo useradd -m -s /usr/sbin/nologin slavyanka-tester1
sudo install -d -m 700 -o slavyanka-tester1 -g slavyanka-tester1 \
  /home/slavyanka-tester1/.ssh
# Paste the tester1 pubkey:
sudo -u slavyanka-tester1 tee /home/slavyanka-tester1/.ssh/authorized_keys <<EOF
ssh-ed25519 AAAA...  slavyanka-tester1
EOF
sudo chmod 600 /home/slavyanka-tester1/.ssh/authorized_keys
```

Make sure sshd allows reverse forwards:

```bash
sudo grep -E '^(AllowTcpForwarding|GatewayPorts)' /etc/ssh/sshd_config
# If GatewayPorts is `no`, the forward binds 127.0.0.1 only — fine if we ssh
# through the LM with ProxyJump. To allow direct dialing from the WAN, set:
#   GatewayPorts clientspecified
# then `sudo systemctl reload sshd`.
```

### 3. Drop `slavyanka.toml` on tester1

Default path: `C:\ProgramData\mirage\slavyanka.toml`. Example:

```toml
jump_host         = "lm-nl-1.zen-geldmaschine.net:22"
jump_user         = "slavyanka-tester1"
ssh_priv_key_path = "C:/ProgramData/mirage/slavyanka_ed25519"
reverse_port      = 22500
target_local_port = 22

# Optional: tunnel the outbound SSH through mirage-client-cli's local SOCKS5.
# Mirage chain config is materialised by mirage-l0-sentinel; the cli's default
# SOCKS5 listener is configured in `client-config.json`. Comment out for direct.
socks5_proxy      = "127.0.0.1:10800"

# Reconnect backoff (1s → 60s by default).
initial_backoff_secs = 1
max_backoff_secs     = 60
```

### 4. Run

```powershell
"C:\Program Files\Mirage\mirage-slavyanka.exe" --config C:\ProgramData\mirage\slavyanka.toml
```

In production sentry (`mirage-sentry`) supervises this — you don't run it by
hand. For first-time smoke test, run it foreground and watch the logs.

### 5. Land on tester1 from the dev box

```bash
ssh -p 22500 -i ~/.ssh/operator-key <tester1-windows-user>@lm-nl-1.zen-geldmaschine.net
```

If `GatewayPorts` is `no` on the LM, ssh into the LM first then `ssh -p 22500
localhost`, or use a single-line ProxyJump:

```bash
ssh -J lm-admin@lm-nl-1.zen-geldmaschine.net \
    -p 22500 \
    -i ~/.ssh/operator-key \
    <tester1-windows-user>@127.0.0.1
```

## Build

```bash
cd nemo/mirage2app
cargo check -p mirage-slavyanka            # local dev host
# Win cross-build for the tester:
bash nemo/scripts/build-win-bins.sh tester1 slavyanka  # requires script update
```

The build-win-bins.sh helper doesn't yet have a `slavyanka` token in its
case-switch — add it (the section near `debug-shell)`) when we cut the first
real Win release. Until then build with:

```bash
cargo build --release -p mirage-slavyanka \
  --target-dir target-build/win-tester1/mirage2app \
  --config 'target."cfg(any(windows, unix))".rustflags=["-C","target-cpu=x86-64-v2"]'
```

The `x86-64-v2` target-cpu is mandatory for tester1's Haswell — see
`mirage2app/CLAUDE.md` *Testers — target CPU compatibility*.

## What slavyanka does NOT do (and what to use instead)

| Want | Use |
|------|-----|
| Signed-manifest spawn / hardware token | Not yet implemented anywhere; see `admin-layer-concept.md` |
| Operator-triggered debug shell on a Mirage client | Existing `mirage-debug-shell` (sentinel-spawned, WS-pushed key) |
| Probe / SSH onto VPS nodes | `mash-ssh-config` ProxyJump aliases + provider SSH keys |

## Caveats / TODOs

- **russh forwarded-channel handling** (`lib.rs::drive_session`) currently
  relies on the OpenSSH server's standard `tcpip-forward` behaviour and russh
  0.60's default channel acceptance. If a real-world test on lm-nl-1 shows
  dropped channels, wire `Handler::server_channel_open_forwarded_tcpip` in
  `InsecureClient` to bridge each channel into a fresh `TcpStream` to
  `127.0.0.1:target_local_port`.
- **No host-key pinning.** Trusted-box mode. If we ever ship this to less
  trusted hosts, pin the LM's pubkey in `InsecureClient::check_server_key`.
- The plain `russh::Disconnect::ByApplication` shutdown path is best-effort —
  if the SOCKS5 hop is dead the disconnect packet won't reach the LM. It will
  time out on the LM's side via `ClientAliveInterval`.
