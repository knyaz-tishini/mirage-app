# CHANGELOG — mirage2app

## Стабильный: e3c4b18 (24 апр 2026)

`docs: README — port map, expected IPs, config reference`

5 чейнов работают через CLI + TUN. socks5_fail=0. Протестировано E2E.

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
11. **49c9971** (24 апр) — mirage-client-core перенесён из mirage2web, v1 GUI → _deprecated
12. **860de1a** (24 апр) — fix(onion): connect timeouts 10s TCP + 15s full handshake (ephemeral + telescopic)
13. **e3c4b18** (24 апр) — docs: README — port map, expected IPs, config reference

---

## Что сделано

- **GUI клиент** — v1 (Windows desktop), v2 (mirage2light), Amnezia-style навигация, connect screen, admin panel
- **CLI клиент** — mirage-client-cli, parallel pool init, default_chain, per-chain SOCKS5 listeners
- **Onion timeouts** — TcpStream::connect 10s timeout + OnionTunnel::connect 15s outer timeout. Предотвращает зависание CLI при недоступных hop нодах
- **Workspace restructure** — mirage-client-core перенесён из mirage2web. mirage2app теперь владеет всеми клиентскими крейтами (cli + client-core + GUI)
