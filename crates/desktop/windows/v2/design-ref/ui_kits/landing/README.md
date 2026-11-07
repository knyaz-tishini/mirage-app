# Landing UI kit

Faithful recreation of the Mirage landing, redirected toward the ops/brutalist direction.

**Components:**
- `Atoms.jsx` — Logo, Icon, Button, Eyebrow, LayerChip, StatusPill, KPI, Header
- `Hero.jsx` — split hero (left: title + KPIs, right: live terminal/config)
- `HowItWorks.jsx` — ASCII mesh diagram + 3 numbered steps
- `Providers.jsx` — dense L1/L2 provider table with tabs
- `Footer.jsx` — Ticker (looping), CTA, Footer

Open `index.html` to see the assembled page. Components are loaded as separate `<script type="text/babel">` files and share via `window` assignment.
