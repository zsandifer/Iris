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
# Only root may be able to change what this script trusts: the paths must be
# real (no symlinks anywhere) and every existing component owned by root and
# not writable by group or others. Otherwise the agent user could swap the
# helper binary, or point the engines directory at something root would then
# hand over to the admin user.
require_root_chain() { # path, what it is
    case "$1" in
        /*) ;;
        *) echo "Give the $2 as an absolute path." >&2; exit 1 ;;
    esac
    p="$1"
    while [ ! -e "$p" ] && [ ! -L "$p" ]; do p="$(dirname "$p")"; done
    if [ "$(readlink -f "$p")" != "$p" ]; then
        echo "The $2 must be a real path without symlinks ($p is one, or is inside one)." >&2
        exit 1
    fi
    while :; do
        if [ "$(stat -c %u "$p")" != "0" ] || [ $(( 0$(stat -c %a "$p") & 022 )) -ne 0 ]; then
            echo "$p must be owned by root and not writable by group or others, so the agent user cannot tamper with the $2." >&2
            exit 1
        fi
        [ "$p" = / ] && break
        p="$(dirname "$p")"
    done
}
# The sudo rule runs this file as the admin user.
require_root_chain "$IRIS_BIN" "iris binary"
# The engines directory itself is the admin user's after a first run.
if [ -e "$ENGINES_DIR" ] || [ -L "$ENGINES_DIR" ]; then
    if [ -L "$ENGINES_DIR" ] || [ ! -d "$ENGINES_DIR" ]; then
        echo "$ENGINES_DIR must be a directory, not a symlink or file." >&2
        exit 1
    fi
    owner="$(stat -c %U "$ENGINES_DIR")"
    if [ "$owner" != root ] && [ "$owner" != "$ADMIN_USER" ]; then
        echo "$ENGINES_DIR is owned by $owner; use a new directory." >&2
        exit 1
    fi
fi
require_root_chain "$(dirname "$ENGINES_DIR")" "engines directory"

# 1. The admin user: no login shell, owns the engines.
if ! id "$ADMIN_USER" >/dev/null 2>&1; then
    useradd --system --home-dir "$ENGINES_DIR" --no-create-home \
        --shell /usr/sbin/nologin "$ADMIN_USER"
fi

# 2. The engines directory: writable by the admin user only, readable and
#    executable by everyone (so the agent user can run engines).
install -d -m 0755 -o "$ADMIN_USER" -g "$ADMIN_USER" "$ENGINES_DIR"

# 3. Let the agent user run exactly `iris engines <op> <name> --dir
#    <engines-dir>` as the admin user. `iris engines` accepts only the four
#    known engine names, fetches only from their official sources, and
#    refuses an engines directory the agent user could tamper with.
SUDOERS=/etc/sudoers.d/iris-engines
write_rule() {
    rm -f "$SUDOERS.tmp"
    {
        echo "# Installed by Iris deploy/setup-engines.sh"
        # Classic sudo on some distributions requires a terminal by default;
        # sudo-rs has no such setting and rejects the line.
        if [ "${1:-}" = tty ]; then
            echo "Defaults:$AGENT_USER !requiretty"
        fi
        # Every allowed command spelled out: no wildcards, so the engines
        # directory cannot be swapped for another one.
        printf '%s ALL=(%s) NOPASSWD: %s engines list --dir %s' \
            "$AGENT_USER" "$ADMIN_USER" "$IRIS_BIN" "$ENGINES_DIR"
        for op in install update uninstall; do
            for engine in claude codex gemini opencode; do
                printf ', %s engines %s %s --dir %s' "$IRIS_BIN" "$op" "$engine" "$ENGINES_DIR"
            done
        done
        echo
    } > "$SUDOERS.tmp"
    chmod 0440 "$SUDOERS.tmp"
}
write_rule tty
if command -v visudo >/dev/null 2>&1 && ! visudo -cf "$SUDOERS.tmp" >/dev/null 2>&1; then
    write_rule
    visudo -cf "$SUDOERS.tmp" >/dev/null
fi
mv "$SUDOERS.tmp" "$SUDOERS"

# 4. The bundled user unit sets NoNewPrivileges=yes, which blocks sudo.
#    Relax only that, for this user's Iris service.
#    The agent user controls its home directory (and could plant symlinks
#    there), so the drop-in is written as that user, never as root.
AGENT_HOME="$(getent passwd "$AGENT_USER" | cut -d: -f6)"
DROPIN_DIR="$AGENT_HOME/.config/systemd/user/iris.service.d"
DROPIN="# Installed by Iris deploy/setup-engines.sh: allow 'sudo -u $ADMIN_USER iris engines'.
[Service]
NoNewPrivileges=no"
if command -v runuser >/dev/null 2>&1; then
    AS_AGENT="runuser -u $AGENT_USER --"
elif command -v setpriv >/dev/null 2>&1; then
    AS_AGENT="setpriv --reuid=$AGENT_USER --regid=$(id -g "$AGENT_USER") --init-groups --"
else
    echo "Need runuser or setpriv (util-linux) to write the agent user's files safely." >&2
    exit 1
fi
printf '%s\n' "$DROPIN" | $AS_AGENT sh -c 'umask 022 && mkdir -p "$1" && cat > "$1/engines.conf"' sh "$DROPIN_DIR"

echo "Done. Add to $AGENT_HOME/.config/iris/config.toml:"
echo ""
echo "  [engines]"
echo "  dir = \"$ENGINES_DIR\""
echo "  helper = [\"sudo\", \"-n\", \"-u\", \"$ADMIN_USER\", \"$IRIS_BIN\", \"engines\"]"
echo ""
echo "Then, as $AGENT_USER: systemctl --user daemon-reload && systemctl --user restart iris"
echo "Engines need Node.js (npm) on the server for Codex, Gemini CLI, and OpenCode."
