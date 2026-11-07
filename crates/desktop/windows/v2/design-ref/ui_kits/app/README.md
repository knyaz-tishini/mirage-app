# App — Mirage (mobile)

Single-button connect screen. Everything else is invisible — config, node selection, keys — all handled server-side.

## Files
- `index.html` — mounts inside an iOS frame
- `MirageApp.jsx` — screen components (top strip, mesh rings, connect button, route, stats, microlog)
- `ios-frame.jsx` — device chrome (starter)

## States
- `off` — standby. Mesh rings are muted, button "CONNECT"
- `connecting` — 1.6s animation. Button "HANDSHAKE…", logs show handshake lines
- `on` — amber glow, button "DISCONNECT", rotating highlight around L1/L2 rings, live ping/down/up numbers

## Screen anatomy (top → bottom)
1. Top strip: logo + session id + on/neg/off indicator
2. **Mesh diagram** — user dot center → 3 L1 (blue squares) → 6 L2 (amber circles) on outer ring; tick marks; halo rings when on
3. **Big button** — 220×60, accent when on, outline when off; 4px radius (brutalist, not pill)
4. Route line: origin → L1 → L2 (with dashes when off)
5. Live stats: ping / down / up (mono tabular)
6. Microlog: 3–4 streaming OK lines
7. Bottom anchor: mesh id + version

## Brand hits
- Mono-first (JetBrains Mono) for every number, timestamp, label
- Space Grotesk for `mirage` wordmark
- Dot-grid background
- Amber (`#FBB26A`) only for the active connect state
- Blue (`#6DB8F5`) reserved for L1 markers
- No emoji, no rounded-pill fluff

## Not included (by design)
No server list, no country picker, no settings. The brand promise is: user doesn't configure anything. If you need to add something, add it as a detail pane on the dashboard, not here.
