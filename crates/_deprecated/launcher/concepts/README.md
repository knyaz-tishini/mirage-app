# Mirage client — routing/mesh viz concepts

Conceptual rendering assets for the dancing-cube launcher's routing/mesh views.
Each panel is a self-contained generative visualization (HTML + Canvas2D, zero-dep),
ported in spirit from the **AT-ORR instrument palette** and re-parameterized on real
Mirage client data (`app → hops → destination`, mesh nodes by region).

These are **design probes**, not shipped code. They render in the visualize sandbox
today; the chosen ones get **rewritten in Rust on uzor + tiny-skia** for the launcher
(`draw_mesh_wheel` / a new routing-tree widget). Canvas2D maps 1:1 to tiny-skia:
`moveTo/lineTo/stroke` → `PathBuilder` + `Paint`, `arc/fill` → circle path, `font/fillText`
→ uzor text. The `state → step(dt) → draw(ctx)` loop maps to the launcher's frame tick.

Open `gallery.html` in a browser to see them all live, or render via visualize.

## Data contract (what the launcher model already provides)

`model.flows[]` = `{chain_id, domain, dest_ip, dest_port, pid, exe_path, bytes_per_sec}`
`model.chains[]` = `{chain_id, hops:[{role,node_id,display_name,region,addr}], protocol, healthy, latency_ms, bytes_per_sec}`
`model.mesh.nodes[]` = `{node_id, region, addr, labels}`

So every panel can bind to live state: nodes = mesh/hops, edges = chain topology,
the flowing "packet/bead" speed = `bytes_per_sec`, color = protocol/health.

## Concept catalog

| # | Panel | Source instrument | Algorithm | Mirage mapping | uzor port notes |
|---|-------|-------------------|-----------|----------------|-----------------|
| 1 | Octolinear trace | Gilded Circuit (#58) + Transit Concordance (#46) | 45°-only routing + flowing bead | hops = vias, exit = pad, bead = live packet (bytes/s) | trivial: polyline + circles; 45° grid snap. **Recommended base grammar** — kills the diagonal spaghetti |
| 2 | Hyphae tree | Hyphae Registry (#12) | space colonization | root = app, "food" near destinations, canopy = answered routes | grow incrementally per tick; cache segments; organic but busy — good for "discovering routes" anim |
| 3 | Constellation mesh | Constellation Bureau (#70) | nearest-neighbour star chains + naming | stars = mesh nodes, constellation = active chain, name = chain_id | twinkle = liveness; meteor = new flow; named figure per chain |
| 4 | Neon ribbons | Light Eels (#59) | additive follow-the-leader ribbons | one ribbon per active chain, head = packet, glow = throughput | needs `lighter` blend = additive alpha accumulation in tiny-skia; gorgeous for "connected/active" |
| 5 | Orbital 3D tree | Torus Reliquary (#32) projection | perspective-projected radial tree, drag-to-spin | root center → hop shells → exit shell → dest, beads flow outward | **the "3D interactive tree"** — pure CPU projection, no GPU; depth-sort draw order; drag = rotate |
| 6 | Geo mesh globe | Census Globe (#33) | golden-angle dot sphere, ortho proj | fleet nodes at real lat/lon, chains = great-circle arcs | drag to spin; back-face cull; region becomes literal geography. Best for a mesh-overview tab |

## Not yet probed (candidates)

- Curl-noise flow field (Weather Station #22) — traffic as advected streams toward exits.
- Metro multi-line (Transit Concordance #46, full) — shared nodes = interchange stations.
- Aurora curtains (Aurora Veil #45) — ambient idle/connecting backdrop.
- Isoline radar (Isoline Survey #9 / Meridian Radar #28) — tasteful replacement for the removed doppler radar.

Source collection: `at-orr` (73 instruments, MIT). See its `README.md` for per-panel papers.

## v2 — re-imagined interactive set (CURRENT DIRECTION → `reimagined.html`)

Owner review of v1: only the **globe** was acceptable. New direction = the globe as the
interactive hero + a small focused set, **all genuinely interactive**, all in the real
**Mirage identity palette** (from `launcher/src/main.rs`):

| token | hex | use |
|-------|-----|-----|
| bg | `#0B0B0E` | canvas |
| surface | `#14151A` | cards |
| FG0 | `#F4F4F5` | near-white text / land dots |
| FG1 | `#878B91` | muted brand grey (sea dots, idle) |
| FG3 | `#555860` | HUD / dim |
| ACCENT (amber) | `#FBB26A` | hero accent: nodes, arcs, active |
| amber-hi | `#FFD9B0` | flowing beads / highlight |
| RED | `#E05656` | alert only |

Discipline: amber + white + grey on near-black; red only for alerts. **No hue rainbow** —
differentiate by brightness/opacity, not by color. State encodes color (healthy=amber,
idle=grey, error=red).

| Panel | Source | Interaction | Mirage role |
|-------|--------|-------------|-------------|
| Mesh globe (hero) | Census Globe #33 | drag-rotate + inertia, **wheel zoom**, flick-to-spin, dbl-click reset, labels fade in on zoom | mesh/topology overview; nodes at real lat/lon, chains = great-circle arcs + beads |
| Bloom A · Dahlia | Dahlia Synod #49 | click = connect/disconnect (opens/closes), drag = curl petals | **cube replacement** — centerpiece opens when connected, breathes when on |
| Bloom B · Peony | Peony Reliquary #48 | click = connect, drag = bloom amount | alt cube replacement (layered petals) |
| Fleet relief | Block Survey #34 | drag to raise/probe | axonometric heightfield = per-node load/latency telemetry |
| Link matrix | Glyph Census #37 | click = rescramble a district | connections/credential matrix; ratified (amber/white/red) cells = active flows/states |

Open `reimagined.html` standalone, or render via visualize. v1's `gallery.html` (6 probes)
kept for reference but superseded except the globe.

## v3 — centerpiece morph scene (CURRENT → `morph-scene.html`)

Owner's idea for the cube replacement: **one continuous scene that morphs with connection
state**, not a static centerpiece. State machine:

1. **BOOT** — a **white relief** (Block Survey #34 heightfield) gently undulating.
2. **READY** (services connected) — relief turns **amber** and writhes more actively.
3. **COLLAPSE** (click the scene) — the relief folds into a **sphere** (lat/lon grid wraps:
   grid `(u,v)` → `lon=u·2π, lat=(v−0.5)·π`; per-vertex `lerp(relief, sphere, fold)`).
4. **MESH** (all connected) — the mesh nodes + great-circle arcs + beads fade in **on the
   sphere** (= the current mesh globe), grid wireframe dims under them.

One shared camera (drag-rotate + inertia, wheel zoom) the whole way through. Auto-cycles for
demo; `boot/services up/collapse/connected` buttons + `auto` toggle drive it manually; click
the scene to collapse. This unifies two earlier cube-replacement variants (fleet relief +
globe) into a single object. **Primary candidate.**

Owner verdict: **morph scene is the keeper**, iterate on it. Blooms (peony/dahlia) were
rejected as cube replacement and removed from the file — the ellipse approximation lost the
source's bezier petals + buffer-rasterizer pipeline; not worth pursuing now that morph won.

uzor/tiny-skia port note: the relief↔sphere morph is a per-vertex `lerp(relief, sphere, fold)`
on the same projected-grid draw as the globe — pure CPU, no GPU. State machine = the launcher's
connection phase (`boot → ready → collapse(click) → mesh`).

**v3.1 material pass** (`morph-scene.html`): bare wireframe → **shaded solid facets**. Each grid
quad gets a face normal (cross of edges) · Lambert shade against a fixed light, painter's-sorted
by depth (back-to-front) so the sphere self-occludes; colour = `lerp(white, amber)` per quad via
a **radial sweep** (amber fills from centre outward as `colT` rises = services coming up); writhe
got a second noise octave. Mesh arcs are front-face culled (`z >= -0.02`) so back arcs hide
behind the solid sphere.

## Alt aesthetic line — bloom variant (`bloom-variant.html`)

Owner: keep the morph scene as the primary centerpiece; the bloom is a **separate alternative
aesthetic line** (scenario TBD, **no traffic viz** — purely decorative). Built on the REAL flora
pipeline this time (previous ellipse attempt was rejected):

- one **model buffer** (240×180): a peony drawn with the source's bezier `petal()` + ring layout,
  amber cultivar (outer `#FFE0BC` → inner `#FB A8 60` → deep `#CE7434`, core `#784A1E`).
- **three rasterizers off the same buffer** (the "buffer trick"): PIXEL / HALFTONE / ASCII
  (`rasterize()` samples on a `step` grid → squares / luminance-dots / `RAMP` glyphs). HALFTONE =
  the look in the owner's reference screenshot.
- lifecycle (Wilt Chronicle #50): on **ready** the flower **grows out of a scatter of pixels**
  (`growT` eases 0→1; pre-grow dim seed dots); then it **opens/closes by LOCAL TIME** — `bloom =
  0.5−0.5·cos(2π·(secs mod period)/period)`, anchored to the wall clock (HH:MM:SS shown).

uzor/tiny-skia port: bezier petal = `PathBuilder` quads; the buffer→rasterizer is a downsample +
per-cell circle/rect/glyph — cheap on tiny-skia. `new Date()` → system clock in Rust.
