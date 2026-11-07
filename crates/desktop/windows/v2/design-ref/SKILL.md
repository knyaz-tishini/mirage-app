---
name: mirage-design
description: Use this skill to generate well-branded interfaces and assets for Mirage, either for production or throwaway prototypes/mocks/etc. Contains essential design guidelines, colors, type, fonts, assets, and UI kit components for prototyping.
user-invocable: true
---

Read the README.md file within this skill, and explore the other available files.

Key files:
- `README.md` — brand understanding, content fundamentals, visual foundations, iconography
- `colors_and_type.css` — all tokens (colors, type, spacing, radii, shadows)
- `fonts/` — self-hosted PT Root UI, JetBrains Mono, Roboto
- `assets/` — `logo.svg`, `icons/` (29 SVGs, 1.5px stroke, amber accent)
- `preview/` — small HTML cards documenting the design system
- `ui_kits/landing/` — marketing site components
- `ui_kits/app/` — mobile client frame (legacy admin mockup removed)

## Critical brand rules
- **Never call Mirage a VPN.** Mirage sells *configuration + operation* of L1/L2 nodes (white-list relays in RU, obfuscated exits abroad). Copy: "mesh", "route", "узел", "обфускация трафика", "конфигурация и обслуживание" — never "VPN", "сервер", "VPS".
- Russian-first copy. English tech terms OK (L1, L2, XTLS-Vision, REALITY, mTLS).
- Mono-first for all numbers, IPs, timestamps, labels. Space Grotesk for display only.
- Amber `#FBB26A` is the single brand accent. Blue `#6DB8F5` reserved exclusively for L1 relays.
- Dark-first (`--bg: #0E0E11`). No light mode.
- Hairline borders everywhere, no card shadows. Brutalist grid aesthetic.
- No emoji. No rounded pill buttons for primary CTAs. Radius = 4-8px, not 12+.
- Dot grid / line grid for section backgrounds (`--grid-dot`, `--grid-size`).

## If creating visual artifacts
Copy assets out (logo, icons, fonts) and create static HTML files. Link `colors_and_type.css` from the skill root for instant brand fidelity. Use UI kit components as reference — don't reinvent.

## If working on production code
Lift tokens from `colors_and_type.css`, mirror component patterns from the relevant UI kit, and keep the ops-brutalist rhythm: mono numbers, hairline rulers, status dots, terminal-style log rows.

## If invoked without guidance
Ask what the user wants to build or design — marketing page, dashboard panel, app screen, slide deck, social asset. Ask 3-5 targeted questions (surface, audience, content density, whether it's for production or throwaway). Then act as an expert Mirage designer producing HTML artifacts or production code.
