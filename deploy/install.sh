#!/bin/sh
# Installs or updates airbnb-notifier as a systemd service.
#   curl -fsSL https://raw.githubusercontent.com/claude-bot-nikitos/airbnb-notifier/main/deploy/install.sh | sudo sh
# Set BINARY=/path/to/airbnb-notifier to install a locally built binary instead of a release.
set -eu

REPO="claude-bot-nikitos/airbnb-notifier"
DIR="/opt/airbnb-notifier"
BIN="$DIR/airbnb-notifier"
CONFIG="$DIR/config.toml"

if [ "$(id -u)" -ne 0 ]; then
    echo "Please run as root (sudo)." >&2
    exit 1
fi

case "$(uname -m)" in
    aarch64 | arm64) TARGET="aarch64-unknown-linux-musl" ;;
    x86_64 | amd64) TARGET="x86_64-unknown-linux-musl" ;;
    armv7l) TARGET="armv7-unknown-linux-musleabihf" ;;
    *) echo "Unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

mkdir -p "$DIR"
if [ -n "${BINARY:-}" ]; then
    cp "$BINARY" "$BIN.new"
else
    URL="https://github.com/$REPO/releases/latest/download/airbnb-notifier-$TARGET"
    echo "Downloading $URL"
    curl -fsSL -o "$BIN.new" "$URL"
fi
chmod 755 "$BIN.new"
mv "$BIN.new" "$BIN"

# Wrapper so `airbnb-notifier user add ...` works from anywhere.
cat > /usr/local/bin/airbnb-notifier <<WRAPPER
#!/bin/sh
export AIRBNB_NOTIFIER_CONFIG="\${AIRBNB_NOTIFIER_CONFIG:-$CONFIG}"
exec $BIN "\$@"
WRAPPER
chmod 755 /usr/local/bin/airbnb-notifier

cat > /etc/systemd/system/airbnb-notifier.service <<UNIT
[Unit]
Description=airbnb-notifier - Telegram alerts for new Airbnb listings
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=$DIR
ExecStart=$BIN --config $CONFIG run
Restart=always
RestartSec=10
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=$DIR

[Install]
WantedBy=multi-user.target
UNIT
systemctl daemon-reload

if [ ! -f "$CONFIG" ]; then
    "$BIN" --config "$CONFIG" init
fi

if grep -q '^bot_token = ""' "$CONFIG"; then
    cat <<MSG

Installed. Finish setup:
  1. sudo airbnb-notifier init --token <token from @BotFather>
  2. sudo systemctl enable --now airbnb-notifier
  3. Send /start to your bot - it replies with your Telegram ID
  4. sudo airbnb-notifier user add <your id>
MSG
else
    systemctl enable airbnb-notifier >/dev/null 2>&1
    systemctl restart airbnb-notifier
    echo "Updated and restarted. Logs: journalctl -u airbnb-notifier -f"
fi
