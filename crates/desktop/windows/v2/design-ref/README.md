# Mirage — Design System

> **Mirage** — сервис, который настраивает и обслуживает вашу частную mesh‑инфраструктуру: конфигурация L1…Lₙ нод, обфускация трафика, активная защита, обновление, мониторинг, поддержка.
>
> ⚠️ **Это не VPN.** Mirage не продаёт VPS напрямую — мы аггрегируем проверенных провайдеров и настраиваем на их серверах двухузловую маршрутизацию с маскировкой трафика. Пользователь сам владеет инфраструктурой.

---

## Sources

| Asset | Location | Status |
|---|---|---|
| Landing codebase | `github.com/knyaz-tishini/mirage2landing` @ `master` | Mirrored in `reference/landing/` |
| Live URL | `https://m1r4g3.bid` | External |
| Service manifest | `.nemo-service.toml` (mirage2landing) | Served by `mirage2server` via tower_http |

The landing is the **only** design surface provided for this system. Everything in this kit is extracted from `reference/landing/style.css` (tokens) and `reference/landing/index.html` + `app.js` (components, copy, structure).

---

## Product context

Mirage sells **configuration + operations**, not hardware. The product pitch on the landing:

1. **Выберите VPS** — pick an L1 (relay) and L2 (exit) provider from an aggregated catalogue.
2. **Мы настроим** — Mirage deploys WireGuard + two‑hop routing + traffic obfuscation on your servers.
3. **Подключайтесь** — you connect through the Mirage client.

L1 = промежуточный узел (обычно РФ‑юрисдикция, белосписочный IP).
L2 = выходной узел (офшорная юрисдикция, не в РКН‑блоклисте).
The two‑hop topology is the **core visual metaphor** of the brand — user → L1 → L2 → Интернет.

Footer tagline sums up the positioning: *«Мы не продаём VPS. Мы настраиваем вашу инфраструктуру.»*

---

## What's in this folder

```
README.md                ← you are here
SKILL.md                 ← skill manifest for Claude Code
colors_and_type.css      ← CSS variables: palette, type scale, spacing, radii, shadows
assets/                  ← logos, icon SVGs lifted from the landing
reference/landing/       ← verbatim copy of the shipped landing page (source of truth)
preview/                 ← design-system preview cards (shown in the Design System tab)
ui_kits/
  landing/               ← pixel-faithful React recreation of the landing
    README.md
    index.html
    components/*.jsx
```

---

## Content fundamentals

**Language.** All product copy ships in **Russian**. Technical terms (WireGuard, L1, L2, mesh, VPS, ТСПУ, РКН) stay in their native form — do not translate them into Russian when they already travel in English/Latin.

**Tone.** Technical, straightforward, engineer‑to‑engineer. No marketing bombast, no emoji, no exclamation points. Sentences are short and declarative. When a feature has a caveat, it's stated plainly — e.g. a provider card can carry a `Нестабильный статус` warning badge right alongside the rest of its metadata.

**Person.** Addresses the reader as **«Вы»** / **«вашу»** (polite second person). Never «ты». Mirage speaks about itself as **«мы»** — "Мы настроим", "Наш клиент". This is a small, trusted team, not a faceless brand.

**Casing.**
- Section titles: Sentence case in Russian (e.g. «Как это работает», «Провайдеры VPS»).
- Labels / chips / step numbers / node names: **UPPERCASE** with `letter-spacing: 0.04–0.08em` — `L1`, `L2`, `01`, `02`, `ВЫ`, `L1 RELAY`, `ИНТЕРНЕТ`.
- Button labels: Sentence case — «Скачать клиент», «Выбрать провайдеров», «Перейти».
- Prices: unit suffix attached, never abbreviated — «от 300 ₽/мес», «от $2.10/мес», «от €4.59/мес».

**Lexicon.**
- ✅ «Конфигурация частной mesh‑сети», «обфускация трафика», «двухузловая маршрутизация», «выходной узел», «юрисдикция», «белосписочный статус».
- ❌ Never: «VPN», «прокси», «анонимайзер», «обход блокировок». Mirage deliberately sidesteps that framing. The hero avoids the word VPN entirely; the footer explicitly states we do not sell VPS.
- When naming layers, always prefix the letter code — `L1 Relay`, `L2 Exit` — so the topology stays legible.

**Concrete examples** (all verbatim from the landing):
- Hero subtitle: «Конфигурация частной mesh‑сети с обфускацией трафика».
- Step 2 body: «Предоставьте доступ к серверам, мы развернём защищённую конфигурацию с WireGuard, двухузловой маршрутизацией и обфускацией трафика».
- L2 hint: «Главное: офшорная юрисдикция, разрешённый Tor и приватный трафик, отсутствие логирования».
- Provider desc: «Российский хостинг с ДЦ в Нидерландах. Амстердамский ДЦ идеален для exit‑нод».

**Emoji.** Not used anywhere. Status and layer are communicated through coloured badges (see Visual foundations → Badges), never through emoji.

---

## Visual foundations

### Palette

Mirage is a **dark‑first** system; there is no light mode in the codebase. The palette is three layers deep:

- **Ground**: `#0E0E11` (page) → `#1C1D21` (surface) → `#242428` (raised) → `#2C2D30` (hover). Each step is ~4–6 L\* units warmer/lighter than the one below.
- **Accent**: a single **amber** `#FBB26A` with a hover variant `#F9A04E`. Amber carries everything primary — CTA button, hero subtitle, logo, step numbers, L2 layer, pricing amount, provider price chip.
- **Layer‑coded blue** `#6DB8F5` used **only** for the L1 relay (relay nodes and `L1` badges). This split — blue = relay, amber = exit — is load‑bearing; do not swap or reuse these colours elsewhere.
- **Status**: green `#5CB87A` for paid/available ticks, red `#EB5757` for disabled features, warn `#E8A838` for «нестабильный» warnings.
- **Text**: `#D7D8DB` primary, `#878B91` muted. On amber, text is near‑black `#1A1400` — not `#000`.

### Typography

- **PT Root UI** (variable, 300–900) — primary brand sans, used for display, headings, and UI. Self‑hosted from `fonts/PTRootUI-VF.ttf`.
- **Roboto** 400 / 400i / 700 / 700i — secondary/body fallback when PT Root UI metrics aren't ideal (long‑form paragraphs, italic needs). Self‑hosted from `fonts/Roboto-*.ttf`.
- **JetBrains Mono** 400 / 700 — config snippets, keys, IP addresses, code specimens. Self‑hosted.
- **Noto Sans Symbols 2** — glyph‑coverage fallback for arrows and geometric symbols used decoratively in infrastructure diagrams.
- Type scale is in `colors_and_type.css`. The hero title uses `clamp(40px, 6vw, 64px)`, section titles `clamp(28px, 4vw, 40px)`. Body is 16–17px at line‑height 1.6–1.7.
- Labels (step numbers, badge text, node names in the diagram) are `11–13px`, `UPPERCASE`, `letter-spacing 0.04–0.08em`, weight 700.
- PT Root UI is the working family across display and UI. Roboto is the body fallback where italics or long‑form reading are needed. No serif.

### Spacing

- **8‑pt grid.** Tokens: 4 / 8 / 12 / 16 / 20 / 24 / 32 / 40 / 48 / 56 / 64 / 96.
- Sections are `padding: 96px 0` (mobile → 64px).
- Cards are `padding: 24–32px`, gap 16–20px between internal blocks.
- Container max‑width `1160px` with 24px side padding (16px on mobile).

### Backgrounds

No photographic imagery, no hand‑drawn illustration, no patterns or textures, no full‑bleed photography. The system is intentionally abstract and infrastructure‑coded. Two gradient treatments exist:

- **Hero haze**: a soft amber radial `radial-gradient(ellipse, rgba(251,178,106,0.06), transparent 70%)` anchored to the top‑centre — it reads as a glow behind the title without looking like a gradient hero.
- **Section sandwich**: alternating sections use `linear-gradient(180deg, bg 0%, rgba(surface,0.5) 50%, bg 100%)` — a subtle mid‑tone band that separates the section without adding a hard edge.

All visuals are built from SVG stroke icons and the network‑diagram component (see Iconography).

### Borders

- Default hairline: `rgba(255,255,255,0.07)` — visible but never loud.
- Amber hairline: `rgba(251,178,106,0.30)` — step‑card hover, accent chips, L2 layer badge.
- Blue hairline: `rgba(109,184,245,0.30)` — L1 badges and L1 nodes only.
- Buttons use a **1.5px** border (`btn--outline`), cards use **1px**. Never mix.
- Left‑border accent exists in **one** place: the tab‑panel hint (`border-left: 3px solid var(--accent)`). Do not proliferate this pattern onto general cards.

### Radii

- `6px` — chips, small icon buttons, region tags.
- `8px` — buttons, form fields, tab buttons, step icon tile.
- `12px` — cards, panels, hero visual frame.
- `20px` / `pill` — hero badge, pricing "featured" ribbon.

### Shadows

Shadows are **rare**. Four defined tokens:

- `--shadow-card` (`0 2px 12px rgba(0,0,0,0.3)`) — provider card on hover.
- `--shadow-overlay` (`0 4px 24px rgba(0,0,0,0.4)`) — header when the page is scrolled.
- `--shadow-glow` (`0 0 24px rgba(251,178,106,0.25)`) — amber CTA on hover. This is the most distinctive motif: the button doesn't bloom colour, it bloom**s**.
- `--shadow-featured` (`0 0 40px rgba(251,178,106,0.08)`) — pricing card "featured" halo.

Nothing in the system uses an inner shadow or a material‑style elevation stack.

### Hover & press states

- **Links, nav**: muted → primary text colour (`#878B91` → `#D7D8DB`). No underline. 0.2s ease.
- **Primary (amber) button**: fill goes `#FBB26A` → `#F9A04E`, plus the amber glow shadow appears.
- **Outline button**: border and text both colour‑shift to amber — a clean hue swap, no fill change.
- **Cards**:
  - *Step cards* — border shifts to amber hairline and the card lifts `translateY(-4px)`.
  - *Provider cards* — border lightens to `surface-hover`, `translateY(-3px)` lift, plus `--shadow-card`.
  - *Pricing cards* — `translateY(-4px)` only, no border change.
- **Active / press**: every `.btn` applies `transform: translateY(1px)` — a 1px nudge, no scale‑down.
- **Icon button link** (e.g. provider card "Перейти"): hover adds `--shadow-glow-sm` and the dim amber fill.

All transitions are `0.2s ease` (`--transition`). Nav mobile drawer is `0.25s ease`. No bounces, no springs, no long dwells.

### Transparency & blur

- **Header** is the **only** blurred surface: `background: rgba(14,14,17,0.85); backdrop-filter: blur(16px)`. Use blur nowhere else.
- Dimmed fills (`rgba(251,178,106,0.12)`, `rgba(109,184,245,0.15)`) provide tint without opacity on the element itself. Prefer tinted fills over `opacity: 0.x`.
- Disabled content uses `opacity: 0.5` (see `feature-cross` in pricing).

### Badges & chips

Three badge archetypes, all small, all uppercase, all outlined:

- **Layer badge** — `L1` (blue) / `L2` (amber). 10–11px, letter‑spacing 0.04em, tinted fill + same‑hue border.
- **Privacy/payment badge** — green, marks attributes like «Рубли, карты РФ», «USDT», «BTC».
- **Warning badge** — amber `#E8A838`, same shape as privacy. Reserved for «Нестабильный статус» and similar caveats.
- **Hero eyebrow** — pill‑shaped (20px radius), 13px, amber dim fill + border.

Chips (region tags) are the low‑key variant: `rgba(255,255,255,0.04)` fill, muted text, 12px, 4px radius.

### Layout rules

- Single fixed element: the **header** (sticky, blurred, 64px). No bottom nav, no floating buttons.
- Hero is a two‑column grid on desktop; the network‑diagram block is a bordered `surface` tile, never full‑bleed.
- Sections alternate subtly via gradient but never change background colour outright — the whole page reads as one dark ground.
- No cards span edge‑to‑edge of the viewport. Everything stays within the 1160px container.

### Imagery

There is none. The brand leans entirely on type, colour, stroke icons, and the node‑diagram. When imagery is genuinely needed (future product screenshots, team photos), treat it as **cool/neutral** (dark‑mode UI screenshots at minimum), never warm. Avoid grain. Do not use stock photography.

### Animation

- Hover transitions only. 0.2s ease on colour/border/transform/box‑shadow.
- Card lift on hover: `translateY(-3–4px)`, nothing more.
- No scroll‑triggered animations, no number counters, no Lottie, no parallax.
- Burger → close icon is a rotate of the two outer bars + opacity 0 on the middle bar, 0.2s ease.
- Respect `prefers-reduced-motion` when adding motion to this system.

---

## Iconography

**System.** Hand‑rolled **inline SVGs** in the HTML source — no icon font, no sprite, no Lucide, no Heroicons. Every icon is `currentColor`, `stroke-width: 1.8` (or `1.6` for tiny indicators at 12px), `stroke-linecap: round`, `stroke-linejoin: round`. `fill="none"` is the default; a handful of tiny accent dots use `fill="currentColor"`.

**Size conventions.**
- Logo / nav: 28px.
- Node diagram, step card, feature checks: 22–28px.
- Inline link affordances (`Перейти →`), feature ticks in pricing: 12–16px.

**Where icons live in this kit.** `assets/icons/` contains every icon extracted from the landing — the Mirage logo, user / L1 relay / L2 exit / internet nodes, step icons (grid, stack, shield‑check), the arrow connector between steps, the "external link" arrow, the payment card, the warning triangle, the feature check and cross. Reference them as `<img src="assets/icons/<name>.svg">` for the mocking case or inline them when you need to recolour with `currentColor`.

**Emoji.** **Never.** Mirage does not use emoji in any product surface — status is communicated with outlined, coloured badges.

**Unicode as icons.** Not used. The only unicode glyph the landing carries is `©` in the footer copyright.

**When you need an icon that isn't in this kit.** Draw in the same style: 24×24 viewBox, 1.8 stroke, `currentColor`, rounded caps/joins, no fill, no gradient. If you must reach for a library, Lucide is the closest stylistic match — but flag the substitution and don't mix sets.

---

## Font substitution note

The original landing loaded **PT Sans** from Google Fonts. The user has provided self‑hosted **PT Root UI** (variable), **Roboto**, **JetBrains Mono**, and **Noto Sans Symbols 2** — all live in `fonts/`. PT Root UI replaces PT Sans as the primary family (close stylistic match, much wider weight range). If the brand actually wants PT Sans kept, swap `--font-sans` back.

---

## UI kits

| Kit | Path | Covers |
|---|---|---|
| Landing | `ui_kits/landing/` | Header, hero, network diagram, step cards, provider cards & tabs, footer, buttons, badges |
| Mobile app | `ui_kits/app/` | iOS client frame. The legacy admin/dashboard mockup was removed. |

### Mobile app — how modes work

The app is intentionally a **single build with a mode switch** at the top.

- **Client mode** — what end-users see. One big CONNECT button, route line, live ping/down/up, tiny 3-line microlog. Paste-config sheet lets the user paste a `mirage://` ticket — Mirage deploys the route, the user never picks a protocol or region.

This split reflects the product truth: users don't switch protocols, Mirage does. Admins (Mirage team and technical clients) get full observability from the same binary.

---

## Index — what's where

| Path | What it is |
|---|---|
| `README.md` | This file — brand context, content fundamentals, visual foundations, iconography |
| `SKILL.md` | Claude Code skill manifest — load this to become a Mirage designer |
| `colors_and_type.css` | All CSS tokens — palette, type scale, spacing, radii, shadows, semantic vars |
| `fonts/` | Self-hosted PT Root UI (variable), Roboto, JetBrains Mono, Noto Sans Symbols 2 |
| `assets/logo.svg` | Primary Mirage wordmark |
| `assets/icons/` | 29 inline-style SVG icons (1.8px stroke, currentColor, rounded caps) lifted from the landing |
| `reference/landing/` | Verbatim mirror of the shipped landing — source of truth for tokens + copy |
| `preview/` | 16 small HTML cards shown in the Design System tab (colours, type, components, brand) |
| `ui_kits/landing/` | React recreation of the marketing landing — Header, Hero, HowItWorks, Providers, Footer |
| `ui_kits/app/` | iOS client frame (legacy admin mockup removed) |
| `index.html` | Entry point — links to each UI kit and to the preview cards |

## Quick reference

- **Accent amber**: `#FBB26A` • Hover `#F9A04E` • on‑amber text `#1A1400`
- **L1 blue**: `#6DB8F5` • **L2 amber**: `#FBB26A` (same as accent)
- **Surfaces**: `#0E0E11` → `#1C1D21` → `#242428` → `#2C2D30`
- **Text**: `#D7D8DB` primary / `#878B91` muted
- **Radii**: 6 / 8 / 12
- **Family**: PT Root UI (variable) / Roboto / JetBrains Mono
- **Container**: 1160px
