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
