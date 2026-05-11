#!/usr/bin/env bash
# Build mirage-sentry as x86_64-unknown-linux-musl for ONE shortname.
# Slavyanka embeds env/local/slavyanka-<shortname>-ed25519 at compile-time, so
# each shortname gets its own binary with its own SSH key inside.
#
# Usage:
#   bash scripts/build-musl-sentry.sh <shortname> [server-lm-mode|server-node-mode|client-mode]
#
#   bash scripts/build-musl-sentry.sh lm-nl-1 server-lm-mode
#   bash scripts/build-musl-sentry.sh tester2 client-mode
#
# Output: target-build/sentry-<shortname>/x86_64-unknown-linux-musl/release/mirage-sentry
#
# Volumes: mirage2app workspace at /work/mirage2app, env/local at /work/env/local
# (slavyanka's include_str! reaches /work/env/local/slavyanka-<shortname>-ed25519).

set -e

SHORTNAME="${1:?shortname required, e.g. lm-nl-1}"
FEATURE="${2:-server-lm-mode}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
MIRAGE2APP_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
NEMO_DIR="$(cd "$MIRAGE2APP_DIR/.." && pwd)"
MIRAGE2WEB_DIR="$(cd "$NEMO_DIR/mirage2web" && pwd)"
ENV_LOCAL_DIR="$NEMO_DIR/env/local"

KEY_FILE="$ENV_LOCAL_DIR/slavyanka-$SHORTNAME-ed25519"
[ -f "$KEY_FILE" ] || { echo "missing $KEY_FILE — run ssh-keygen first"; exit 1; }

OUT_DIR="$NEMO_DIR/target-build/sentry-$SHORTNAME"
mkdir -p "$OUT_DIR"

echo "[build-musl-sentry] shortname=$SHORTNAME  feature=$FEATURE  out=$OUT_DIR"

# mirage2app/Cargo.toml has path-deps on mirage2web/crates/mirage-{core,protocol,mux},
# so we must mount mirage2web alongside mirage2app under /work.
MSYS_NO_PATHCONV=1 docker run --rm \
  -v "$MIRAGE2APP_DIR:/work/mirage2app" \
  -v "$MIRAGE2WEB_DIR:/work/mirage2web" \
  -v "$ENV_LOCAL_DIR:/work/env/local:ro" \
  -v "$OUT_DIR:/target" \
  -w /work/mirage2app \
  -e CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-cpu=x86-64" \
  -e CARGO_HOME=/cargo-cache \
  -e CARGO_TARGET_DIR=/target \
  -e SENTRY_SHORTNAME="$SHORTNAME" \
  -v "mirage-sentry-cargo-cache:/cargo-cache" \
  messense/rust-musl-cross:x86_64-musl \
  cargo build --release \
    --target x86_64-unknown-linux-musl \
    -p mirage-sentry \
    --no-default-features \
    --features "$FEATURE"

BIN="$OUT_DIR/x86_64-unknown-linux-musl/release/mirage-sentry"
ls -la "$BIN" 2>/dev/null && echo "[build-musl-sentry] OK $SHORTNAME -> $BIN" || { echo "[build-musl-sentry] FAIL"; exit 1; }
