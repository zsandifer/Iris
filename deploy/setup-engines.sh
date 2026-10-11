#!/bin/sh
# One-time root setup for installing engines from the Iris app with a
# privilege split.
#
# Iris (running as your normal user, the "agent user") can then install and
# update the official CLIs (Claude Code, Codex, Gemini CLI, OpenCode) from
# the app, but the downloads and writes happen as a separate system user
# that owns the engines directory. The agent user can run the engines and
# cannot modify them.
#
# Without this script Iris still works in single-user mode: it installs
# engines into its own data directory as the agent user.
#
# Usage (as root):
#   sh deploy/setup-engines.sh <agent-user> [iris-binary] [engines-dir]
# Defaults: iris-binary=/usr/local/bin/iris, engines-dir=/opt/iris/engines
#
# Then add to the agent user's ~/.config/iris/config.toml:
#   [engines]
#   dir = "/opt/iris/engines"
#   helper = ["sudo", "-n", "-u", "iris", "/usr/local/bin/iris", "engines"]
# and restart Iris (systemctl --user restart iris, as that user).
set -eu

AGENT_USER="${1:-}"
IRIS_BIN="${2:-/usr/local/bin/iris}"
ENGINES_DIR="${3:-/opt/iris/engines}"
ADMIN_USER="iris"

if [ "$(id -u)" -ne 0 ]; then
    echo "Run this as root (for example with sudo)." >&2
    exit 1
fi
if [ -z "$AGENT_USER" ] || ! id "$AGENT_USER" >/dev/null 2>&1; then
    echo "Usage: sh $0 <agent-user> [iris-binary] [engines-dir]" >&2
    exit 1
fi
case "$IRIS_BIN$ENGINES_DIR" in
    *[!A-Za-z0-9/._-]*) echo "Paths may only contain letters, digits, / . _ -" >&2; exit 1 ;;
esac
if [ ! -x "$IRIS_BIN" ]; then
    echo "$IRIS_BIN is missing. Install it first, e.g.: install -Dm755 target/release/iris $IRIS_BIN" >&2
    exit 1
fi
if [ "$(stat -c %u "$IRIS_BIN")" != "0" ]; then
    echo "$IRIS_BIN must be owned by root so the agent user cannot replace it." >&2
    exit 1
fi

# 1. The admin user: no login shell, owns the engines.
if ! id "$ADMIN_USER" >/dev/null 2>&1; then
    useradd --system --home-dir "$ENGINES_DIR" --no-create-home \
        --shell /usr/sbin/nologin "$ADMIN_USER"
fi

# 2. The engines directory: writable by the admin user only, readable and
#    executable by everyone (so the agent user can run engines).
install -d -m 0755 -o "$ADMIN_USER" -g "$ADMIN_USER" "$ENGINES_DIR"

# 3. Let the agent user run exactly `iris engines ...` as the admin user.
#    `iris engines` accepts only the four known engine names and fetches
#    only from their official sources.
SUDOERS=/etc/sudoers.d/iris-engines
cat > "$SUDOERS.tmp" <<EOF
# Installed by Iris deploy/setup-engines.sh
Defaults:$AGENT_USER!requiretty
$AGENT_USER ALL=($ADMIN_USER) NOPASSWD: $IRIS_BIN engines *
EOF
chmod 0440 "$SUDOERS.tmp"
if command -v visudo >/dev/null 2>&1; then
    visudo -cf "$SUDOERS.tmp" >/dev/null
fi
mv "$SUDOERS.tmp" "$SUDOERS"

# 4. The bundled user unit sets NoNewPrivileges=yes, which blocks sudo.
#    Relax only that, for this user's Iris service.
AGENT_HOME="$(getent passwd "$AGENT_USER" | cut -d: -f6)"
DROPIN_DIR="$AGENT_HOME/.config/systemd/user/iris.service.d"
install -d -m 0755 -o "$AGENT_USER" "$DROPIN_DIR"
cat > "$DROPIN_DIR/engines.conf" <<EOF
# Installed by Iris deploy/setup-engines.sh: allow 'sudo -u $ADMIN_USER iris engines'.
[Service]
NoNewPrivileges=no
EOF
chown "$AGENT_USER" "$DROPIN_DIR/engines.conf"

echo "Done. Add to $AGENT_HOME/.config/iris/config.toml:"
echo ""
echo "  [engines]"
echo "  dir = \"$ENGINES_DIR\""
echo "  helper = [\"sudo\", \"-n\", \"-u\", \"$ADMIN_USER\", \"$IRIS_BIN\", \"engines\"]"
echo ""
echo "Then, as $AGENT_USER: systemctl --user daemon-reload && systemctl --user restart iris"
echo "Engines need Node.js (npm) on the server for Codex, Gemini CLI, and OpenCode."
