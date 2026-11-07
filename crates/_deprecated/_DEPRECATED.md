# DEPRECATED — crates/_deprecated/launcher (mirage-launcher)

**Retired 2026-09-30. Do not use, do not build, do not reference.**

`mirage-client-gui` (`crates/client-gui/`) is the product GUI. Nothing unique remains in the launcher:
client-gui copied its process lifecycle, and the launcher's mock data carried real fleet IPs and machine ids.
Only its `mirage://` argv handling survives, re-done for `mirage://import` in client-gui (package G5).
