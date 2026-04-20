#!/usr/bin/env bash
# cross-build.sh — build an aarch64 binary for the Jetson Orin Nano.
#
# Prerequisites on the dev machine:
#   cargo install cross --git https://github.com/cross-rs/cross
#   (cross uses Docker, so: `docker` daemon running)
#
# Usage:
#   ./deploy/cross-build.sh              # release build
#   ./deploy/cross-build.sh debug        # debug build (faster compile)
#
set -euo pipefail

PROFILE="${1:-release}"
TARGET="aarch64-unknown-linux-gnu"

if ! command -v cross >/dev/null 2>&1; then
    echo "error: 'cross' not installed. Run:"
    echo "  cargo install cross --git https://github.com/cross-rs/cross"
    exit 1
fi

if [[ "$PROFILE" == "release" ]]; then
    echo "==> Building Augur for $TARGET (release)"
    cross build --target "$TARGET" --release
    BIN="target/$TARGET/release/augur"
else
    echo "==> Building Augur for $TARGET (debug)"
    cross build --target "$TARGET"
    BIN="target/$TARGET/debug/augur"
fi

echo "==> Binary built: $BIN"
ls -lh "$BIN"
file "$BIN"

cat <<INFO

Next steps to deploy to Jetson:

  # On dev machine:
  scp $BIN             jetson:/opt/augur/augur
  scp deploy/.env.example jetson:/opt/augur/.env      # edit with real creds
  scp deploy/augur.service jetson:/tmp/

  # On Jetson:
  sudo useradd -r -s /bin/false augur 2>/dev/null || true
  sudo mkdir -p /opt/augur
  sudo chown -R augur:augur /opt/augur
  sudo chmod 600 /opt/augur/.env
  sudo chmod +x /opt/augur/augur
  sudo mv /tmp/augur.service /etc/systemd/system/
  sudo systemctl daemon-reload
  sudo systemctl enable --now augur
  sudo journalctl -u augur -f
INFO
