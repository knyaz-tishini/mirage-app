# mirage-debug-shell

Reverse SSH tunnel binary spawned by `mirage-l0-sentinel` when the monitor
operator sends a `WsServerFrame::DebugTunnelOpen` frame.

## Goal

Operator on the monitor host wants an SSH session into a remote sentinel
client without inbound connectivity to the client. The client opens a
reverse port-forward to the monitor (`-R <reverse_port>:127.0.0.1:22`),
operator then runs `ssh -p <reverse_port> localhost` on the monitor.

## Usage

```
mirage-debug-shell --config <path-to.json>
```

Config schema (JSON, written by sentinel from the WS frame payload):

```json
{
  "monitor_ssh_host": "lm-tr-1.zen-geldmaschine.net",
  "monitor_ssh_port": 22,
  "monitor_ssh_user": "m2m-debug",
  "ssh_priv_key_pem": "-----BEGIN OPENSSH PRIVATE KEY-----\n...",
  "reverse_port": 22500,
  "local_ssh_port": 22,
  "ttl_secs": 3600
}
```

## Behaviour

1. Writes `ssh_priv_key_pem` to a `0600` temp file in `$TMPDIR`.
2. Spawns `ssh -N -i <key> -R <reverse_port>:127.0.0.1:<local_ssh_port>
   -o StrictHostKeyChecking=no -o ServerAliveInterval=30
   -o ExitOnForwardFailure=yes -p <monitor_ssh_port> <user>@<host>`.
3. Exits when one of:
   - SSH child terminates (any reason).
   - SIGTERM / Ctrl-C received → propagate SIGTERM to child, wait 5s,
     SIGKILL if still alive.
   - `ttl_secs` elapsed → same teardown as SIGTERM.
4. Removes the temp key file on shutdown.

Standard L0 robustness:

- `mirage-runtime-guard` `MemoryGuard` — `exit(0)` on fatal RSS so parent
  (sentinel) can respawn.
- `panic::set_hook` → log + `exit(1)` (no abort).

## Server side

The monitor expects an unprivileged user (default `m2m-debug`) with
`~/.ssh/authorized_keys` populated by the
`POST /relay/clients/{id}/debug/enable` handler. SSH server config should
forbid shell access for that user — only `GatewayPorts no` and TCP
forwarding. Operator separately SSHes through the loopback port.

A practical sshd_config snippet:

```
Match User m2m-debug
  AllowTcpForwarding remote
  GatewayPorts no
  PermitTunnel no
  X11Forwarding no
  AllowAgentForwarding no
  PermitTTY no
  ForceCommand /bin/false
```
