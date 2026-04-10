#!/usr/bin/env bash
# install.sh — Run once on a fresh Linux server.
# Installs dependencies, clones the repo, builds the binary, and sets up a systemd service.
# Safe to re-run: every step is idempotent.

set -euo pipefail

# ─── Config ────────────────────────────────────────────────────────────────────
REPO_URL="https://github.com/ashishact/tts-server.git"
INSTALL_DIR="${INSTALL_DIR:-$HOME/tts-server}"
SERVICE_NAME="tts-server"
SERVICE_USER="$(whoami)"

# ─── Colours ───────────────────────────────────────────────────────────────────
GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; NC='\033[0m'
log()  { echo -e "${GREEN}[install]${NC} $*"; }
warn() { echo -e "${YELLOW}[install]${NC} $*"; }
die()  { echo -e "${RED}[install] ERROR:${NC} $*" >&2; exit 1; }

# ─── 1. System packages ────────────────────────────────────────────────────────
log "Checking system packages..."

if command -v apt-get &>/dev/null; then
  PKGS_NEEDED=()
  for pkg in espeak-ng libespeak-ng-dev pkg-config libssl-dev build-essential cmake git curl clang libclang-dev; do
    dpkg -s "$pkg" &>/dev/null || PKGS_NEEDED+=("$pkg")
  done
  if [ ${#PKGS_NEEDED[@]} -gt 0 ]; then
    log "Installing: ${PKGS_NEEDED[*]}"
    sudo apt-get update -qq
    sudo apt-get install -y "${PKGS_NEEDED[@]}"
  else
    warn "All system packages already installed."
  fi
elif command -v yum &>/dev/null; then
  sudo yum install -y espeak-ng espeak-ng-devel openssl-devel gcc gcc-c++ cmake git curl clang clang-devel
elif command -v dnf &>/dev/null; then
  sudo dnf install -y espeak-ng espeak-ng-devel openssl-devel gcc gcc-c++ cmake git curl clang clang-devel
else
  die "Unsupported package manager. Install manually: espeak-ng, openssl-dev, cmake, gcc, git."
fi

# ─── 2. Rust ───────────────────────────────────────────────────────────────────
if command -v cargo &>/dev/null; then
  warn "Rust already installed: $(rustc --version)"
else
  log "Installing Rust via rustup..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
  export PATH="$HOME/.cargo/bin:$PATH"
fi

# Ensure cargo is available in this shell session
export PATH="$HOME/.cargo/bin:$PATH"
cargo --version &>/dev/null || die "cargo not found after rustup install. Open a new shell and re-run."

# ─── 3. Clone / update repo ────────────────────────────────────────────────────
if [ -d "$INSTALL_DIR/.git" ]; then
  warn "Repo already exists at $INSTALL_DIR — skipping clone."
else
  log "Cloning repo to $INSTALL_DIR..."
  git clone "$REPO_URL" "$INSTALL_DIR"
fi

cd "$INSTALL_DIR"

# ─── 4. .env ───────────────────────────────────────────────────────────────────
if [ -f ".env" ]; then
  warn ".env already exists — not overwriting. Edit it manually if needed."
else
  cp .env.example .env
  log ".env created from .env.example"
  warn "⚠️  Open $INSTALL_DIR/.env and set CONVERSATION_ID, TOKEN, VOICE, etc. before starting."
fi

# ─── 5. Build ──────────────────────────────────────────────────────────────────
log "Building release binary (first build takes ~5-10 min, model downloads on first run)..."

# Override the macOS-specific PKG_CONFIG_PATH from .cargo/config.toml
ARCH="$(uname -m)"
export PKG_CONFIG_PATH="/usr/lib/pkgconfig:/usr/lib/${ARCH}-linux-gnu/pkgconfig:/usr/local/lib/pkgconfig"

cargo build --release

# ─── 6. Systemd service ────────────────────────────────────────────────────────
SERVICE_FILE="/etc/systemd/system/${SERVICE_NAME}.service"

if [ -f "$SERVICE_FILE" ]; then
  warn "Service file already exists at $SERVICE_FILE — not overwriting."
else
  log "Creating systemd service at $SERVICE_FILE..."
  sudo tee "$SERVICE_FILE" > /dev/null <<EOF
[Unit]
Description=Kokoro TTS WebSocket Agent
Documentation=https://github.com/ashishact/tts-server
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${SERVICE_USER}
WorkingDirectory=${INSTALL_DIR}
ExecStart=${INSTALL_DIR}/target/release/tts-server
EnvironmentFile=${INSTALL_DIR}/.env
Environment=RUST_LOG=info

Restart=always
RestartSec=5
StartLimitIntervalSec=60
StartLimitBurst=5

StandardOutput=journal
StandardError=journal
SyslogIdentifier=${SERVICE_NAME}

[Install]
WantedBy=multi-user.target
EOF
fi

sudo systemctl daemon-reload
sudo systemctl enable "$SERVICE_NAME"

# ─── Done ──────────────────────────────────────────────────────────────────────
echo ""
log "✅  Installation complete!"
echo ""
echo "  Next steps:"
echo "    1. Edit your config:      nano ${INSTALL_DIR}/.env"
echo "    2. Start the service:     sudo systemctl start ${SERVICE_NAME}"
echo "    3. Check it is running:   sudo systemctl status ${SERVICE_NAME}"
echo "    4. Follow live logs:      journalctl -u ${SERVICE_NAME} -f"
echo ""
