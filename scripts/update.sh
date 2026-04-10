#!/usr/bin/env bash
# update.sh — Run whenever you pull new changes.
# Pulls latest code, rebuilds the binary, and restarts the service.

set -euo pipefail

# ─── Config ────────────────────────────────────────────────────────────────────
INSTALL_DIR="${INSTALL_DIR:-$HOME/tts-server}"
SERVICE_NAME="tts-server"

# ─── Colours ───────────────────────────────────────────────────────────────────
GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; NC='\033[0m'
log()  { echo -e "${GREEN}[update]${NC} $*"; }
warn() { echo -e "${YELLOW}[update]${NC} $*"; }
die()  { echo -e "${RED}[update] ERROR:${NC} $*" >&2; exit 1; }

# ─── Sanity checks ─────────────────────────────────────────────────────────────
[ -d "$INSTALL_DIR/.git" ] || die "Repo not found at $INSTALL_DIR. Run install.sh first."
command -v cargo &>/dev/null || export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo &>/dev/null || die "cargo not found. Run install.sh first."

cd "$INSTALL_DIR"

# ─── 1. Pull ───────────────────────────────────────────────────────────────────
log "Pulling latest changes..."
git fetch origin
LOCAL=$(git rev-parse HEAD)
REMOTE=$(git rev-parse origin/main)

if [ "$LOCAL" = "$REMOTE" ]; then
  warn "Already up to date ($(git rev-parse --short HEAD))."
  warn "Nothing to rebuild. Exiting."
  exit 0
fi

git pull --ff-only origin main
log "Updated from $(git rev-parse --short "$LOCAL") → $(git rev-parse --short HEAD)"

# ─── 2. Stop service before build ──────────────────────────────────────────────
if systemctl is-active --quiet "$SERVICE_NAME"; then
  log "Stopping service..."
  sudo systemctl stop "$SERVICE_NAME"
fi

# ─── 3. Build ──────────────────────────────────────────────────────────────────
log "Building release binary..."

ARCH="$(uname -m)"
export PKG_CONFIG_PATH="/usr/lib/pkgconfig:/usr/lib/${ARCH}-linux-gnu/pkgconfig:/usr/local/lib/pkgconfig"

cargo build --release

# ─── 4. Restart service ────────────────────────────────────────────────────────
log "Starting service..."
sudo systemctl start "$SERVICE_NAME"

# ─── Done ──────────────────────────────────────────────────────────────────────
echo ""
log "✅  Update complete!  ($(git rev-parse --short HEAD))"
echo ""
echo "  Live logs:   journalctl -u ${SERVICE_NAME} -f"
echo "  Status:      sudo systemctl status ${SERVICE_NAME}"
echo ""
