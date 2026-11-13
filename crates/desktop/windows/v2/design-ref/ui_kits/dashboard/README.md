# Dashboard — Mesh Console

High-density ops console for the Mirage mesh: nodes, live logs, throughput, geo distribution, selected-node detail.

## Files
- `index.html` — full console view
- `kit.css` — dashboard-only styles (top bar, sidebar, KPI strip, node table, logs, chart, map)
- Inherits tokens from `/colors_and_type.css`

## Structure
- **Topbar** — logo, crumb, status, user
- **Sidebar** — Mesh / Config / Ops sections, SVG icons (inline, 1.5px stroke)
- **Page header** — eyebrow, H1 (Space Grotesk 500), actions
- **KPI strip** — 6 cells, mono tabular-nums, status colors
- **Body split** — left: node table + live log tail. right: throughput chart, geo map, selected-node detail

## Visual language
- Grid borders everywhere (hairlines `var(--border)`); no card shadows
- Mono-first: all numbers, addresses, timestamps, pill labels in JetBrains Mono
- Sparklines per-node (8px-bar mini-columns)
- Map is schematic — dot-pattern plate + blob continents + layer-coded markers, not a real geomap
- Status dots: `●` with glow for OK, flat amber for WARN, red for ERR

## Components to lift
- `.topbar`, `.sidebar`, `.sb__item`
- `.kpi-strip`, `.k`
- `.node-row` (grid-template-columns pattern)
- `.log-row` with tag coloring
- `.chart` + inline SVG with L1/L2 stacked bands
- `.btn`, `.btn.primary`
- `.pill`
