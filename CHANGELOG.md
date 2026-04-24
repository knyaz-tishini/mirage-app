# CHANGELOG — mirage2app

## Коммиты

1. **b4072f0** — chore: bump to v0.1.1
2. **3b17319** — feat(nav): compact centered icon bar
3. **c1b7e38** — feat: InputCoordinator, bottom sheet, connect screen, Feather icons (v0.1.2)
4. **e834be0** — feat: 12-screen Amnezia-style navigation (v0.1.3)
5. **946b937** — feat: full Amnezia-depth navigation with real mirage-core protocols (v0.1.4)
6. **0d510a5** — fix: nav bar visible on all screens (v0.1.5)
7. **0dfa3ad** — feat: API client integration with mirage2server (v0.1.6)
8. **b7eda52** — feat: client VPS connect + hidden admin panel (v0.1.7)
9. **309051d** — refactor: restructure mirage2app as unified client workspace
10. **5267e62** — fix(cli): parallel pool init + correct default_chain + rename to mirage-client-cli
11. **(uncommitted)** (24 апр) — mirage-client-core перенесён из mirage2web в mirage2app/crates/client-core

---

## Что сделано

- **GUI клиент** — v1 (Windows desktop), v2 (mirage2light), Amnezia-style навигация, connect screen, admin panel
- **CLI клиент** — mirage-client-cli, parallel pool init, default_chain
- **Workspace restructure** — mirage-client-core перенесён из mirage2web. mirage2app теперь владеет всеми клиентскими крейтами (cli + client-core + GUI)
