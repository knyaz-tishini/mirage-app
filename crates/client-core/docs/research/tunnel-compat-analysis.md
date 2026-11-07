# TunnelCompat Analysis

## 1. How TunnelCompat::wrap() works

`TunnelCompat::wrap(tunnel)` creates a `tokio::io::duplex(65536)` channel yielding two
`DuplexStream` ends: `app_side` and `tunnel_side`.

- `app_side` is stored in `TunnelCompat.stream`. `AsyncRead`/`AsyncWrite` impls on
  `TunnelCompat` delegate directly to it. Callers (MuxSession, relay_loop) read/write here.
- `tunnel_side` is split into `(rd, wr)` and given to a spawned relay task.

**Relay task direction mapping:**

| app writes to TunnelCompat | → DuplexStream internal buffer → relay `rd.read()` → `tunnel.send()` |
|---|---|
| `tunnel.recv()` fires → relay `wr.write_all()` | → DuplexStream internal buffer → app reads from TunnelCompat |

Buffer naming in source is confusing: `wbuf` is used for app→tunnel direction (data read from
`rd` then sent to `tunnel.send`), `rbuf` for tunnel→app direction. Names are swapped from what
you'd expect but logic is correct.

## 2. Bugs in relay task logic

**No bugs found in the data-path logic.** Direction wiring is correct:
- `rd.read(&mut wbuf)` — reads bytes app wrote into the duplex
- `tunnel.send(&wbuf[..n])` — forwards those bytes out through the tunnel
- `tunnel.recv(&mut rbuf)` — receives bytes arriving from tunnel
- `wr.write_all(&rbuf[..n])` — delivers them to the app-side read path

**Potential issue — variable name confusion only:**
`wbuf` holds data being *written to* the tunnel (read from app side).
`rbuf` holds data being *read from* the tunnel (written to app side).
These names are inverted. Not a bug, but maintenance hazard.

**Shutdown asymmetry:** When `rd.read` returns EOF (app dropped write side), the task calls
`tunnel.close()` and breaks. This is correct. When `tunnel.recv` returns 0, the task just
breaks without writing EOF into `wr`. The DuplexStream `wr` will be dropped, which closes
the read side for the app — correct implicit EOF propagation.

## 3. Race condition: relay task vs MuxSession::client() read loop

**Is there a race? Answer: theoretically yes, practically benign.**

`MuxSession::new()` immediately spawns two tasks:
1. `read_loop` — calls `rd.read_exact()` on the transport as soon as it is polled
2. `write_loop` — drains `write_rx` channel

`TunnelCompat::wrap()` calls `tokio::spawn(...)` for the relay task *before* returning the
`TunnelCompat`. However `tokio::spawn` only *schedules* the task; it does not run it
synchronously. The relay task will not execute until the current async task yields.

**Sequence in `build_session`:**
```
let compat = TunnelCompat::wrap(tunnel);   // relay task SCHEDULED, not running yet
Ok(MuxSession::client(compat))             // read_loop + write_loop SCHEDULED
```

After `build_session` returns to the `.await` call site, all three tasks (relay, mux-read,
mux-write) compete for executor time. The mux `read_loop` will call `read_exact` on
`app_side.rd` and block waiting for bytes. The relay task calls `tunnel.recv` and will
deliver bytes into `app_side.wr` when they arrive.

**Why it's benign:** `read_exact` on `DuplexStream` parks the waker. No bytes are consumed
or lost before the relay task starts because the relay task produces bytes only when
`tunnel.recv()` resolves — which requires network I/O. By the time real data arrives from
the network, the relay task is guaranteed to be running (Tokio cooperative scheduler will
poll it at the next yield point, long before network RTT). There is no window where data
could be received and not delivered.

**Not a true race condition** — it would only be a problem if `tunnel.recv()` could return
immediately with buffered data before the relay task ever runs. For all current implementations
(WebSocket, QUIC, ShadowTLS, VLESS, AmneziaWG) this is not possible: they all need at least
one network read before returning data.

**Edge case to watch:** `hysteria2_native`, `quic_xray`, and `websocket_xray` have a
`recv_overflow` buffer. If a previous tunnel use left data in `recv_overflow` (reuse
scenario), `recv()` would return immediately. But `Box<dyn Tunnel>` instances are freshly
constructed per `build_session` call, so `recv_overflow` is always empty at construction.
No actual risk.

## 4. Does MuxSession::client() try to read immediately?

**Yes** — `MuxSession::new()` unconditionally spawns `read_loop` which calls
`rd.read_exact(&mut hdr_buf)` in its first `await`. This is a *blocking poll* on the
DuplexStream read side. It will suspend (not spin) until bytes appear. `open_stream()` is
not required to trigger the read; reading begins the moment the runtime polls `read_loop`.

So reads start immediately, but they park until data flows — which requires the relay task
to be running. Both tasks start effectively simultaneously from the executor's perspective.

## 5. Cancel-safety of tunnel.recv() in tokio::select!

**This is a real concern. Several impls are NOT cancel-safe.**

`tokio::select!` cancels the losing branch by dropping its future. If `tunnel.recv()` is
mid-operation and the `rd.read()` branch wins, the `recv()` future is dropped. On the next
iteration a *new* `recv()` future is created, calling into the same `&mut tunnel` state
machine from the top.

**Per-implementation analysis:**

| Implementation | cancel-safe? | Reason |
|---|---|---|
| `shadowtls_v3` | NO | `recv()` calls `read_exact(&mut header)` then `read_exact(payload)`. If cancelled between those two reads, header bytes are consumed from the TCP stream but the payload read is abandoned. Next `recv()` call reads stale/wrong bytes as a header. **DATA CORRUPTION.** |
| `vless_reality` | NO | Same pattern: reads 2-byte length, then ciphertext. Cancel between the two reads corrupts framing. |
| `websocket_xray` | PARTIAL | `ws.next()` wraps tungstenite which has its own internal read buffer. If the future is cancelled mid-frame, internal tungstenite state may be inconsistent. Depends on tungstenite internals — not guaranteed safe. |
| `hysteria2_native` | NO | Reads 4-byte length prefix then payload. Same two-phase read issue. |
| `quic_xray` | NO | Same 4-byte length + payload pattern. |
| `amneziawg` | SAFE | Single `socket.recv()` call — either completes atomically or not at all (UDP datagram). Cancel-safe. |

**Impact on TunnelCompat relay task:**

The `select!` in the relay task has two branches:
1. `rd.read(&mut wbuf)` — cancel-safe (DuplexStream read is idempotent on cancel)
2. `tunnel.recv(&mut rbuf)` — NOT cancel-safe for TCP-based tunnels

When the `rd.read` branch wins (app sent data), `tunnel.recv` is cancelled. On next loop
iteration, a fresh `tunnel.recv` future is started. For TCP-based tunnels that do two-phase
reads (read header, read body), this can cause:
- Partial header consumption, next recv reads body bytes as a new header
- Silent data corruption or protocol desync
- Eventual parse error and tunnel drop

**Severity:** High for ShadowTLS, VLESS Reality, HYSTERIA2, QUIC transports.
Low for AmneziaWG (UDP, cancel-safe).

**Fix:** Replace `select!` with a proper bidir copy. Options:
1. Use `tokio::io::copy_bidirectional` — but requires `AsyncRead+AsyncWrite` on tunnel
2. Pin the `recv` future across iterations (don't recreate it on each loop)
3. Buffer the recv future result: start `recv` once, only cancel after a timeout, never cancel if data was partially read
4. Wrap tunnel in a task-per-direction instead of single `select!` loop:
   - Task A: `loop { tunnel.recv() → wr.write_all() }` — never cancelled
   - Task B: `loop { rd.read() → tunnel.send() }` — never cancelled
   - This is the correct fix. Two dedicated tasks, no select!, no cancel hazard.

## Summary Table

| Question | Answer |
|---|---|
| wrap() correct direction wiring? | Yes |
| Data-path logic bugs? | No (variable names confusing only) |
| Race condition on startup? | Theoretical only; benign in practice |
| MuxSession reads immediately? | Yes — read_loop starts before open_stream() |
| tunnel.recv() cancel-safe? | NO for ShadowTLS, VLESS, HYSTERIA2, QUIC. YES for AmneziaWG |
| Actual corruption possible? | Yes, for TCP-framed tunnels under bidirectional load |

## Recommended Fix (relay task)

Replace the single `select!` loop with two independent tasks:

```rust
pub fn wrap(tunnel: Box<dyn Tunnel>) -> Self {
    let (app_side, tunnel_side) = tokio::io::duplex(65536);
    let (mut rd, mut wr) = tokio::io::split(tunnel_side);
    let (mut tunnel_rd, mut tunnel_wr) = split_tunnel(tunnel); // needs trait split or Arc<Mutex<>>

    // Task 1: app → tunnel (cancel-safe: DuplexStream read is safe)
    tokio::spawn(async move {
        let mut buf = vec![0u8; 16384];
        loop {
            match rd.read(&mut buf).await {
                Ok(0) | Err(_) => { let _ = tunnel_wr.close().await; break; }
                Ok(n) => { if tunnel_wr.send(&buf[..n]).await.is_err() { break; } }
            }
        }
    });

    // Task 2: tunnel → app (never cancelled = correct)
    tokio::spawn(async move {
        let mut buf = vec![0u8; 16384];
        loop {
            match tunnel_rd.recv(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => { if wr.write_all(&buf[..n]).await.is_err() { break; } }
            }
        }
    });

    Self { stream: app_side, _relay_task: tokio::spawn(async {}) } // placeholder
}
```

The blocker is that `Box<dyn Tunnel>` takes `&mut self` for both `send` and `recv`, so it
cannot be split without either:
- An `Arc<Mutex<Box<dyn Tunnel>>>` wrapper (adds contention but safe)
- A `split() -> (ReadHalf, WriteHalf)` method on the `Tunnel` trait
- Keeping `&mut` exclusive access per task via a channel bridge

Simplest safe approach: wrap tunnel in `Arc<Mutex<>>`, have each task lock briefly per
operation. For typical network I/O this is fine since tasks will be blocked on I/O most
of the time, not competing for the lock.
