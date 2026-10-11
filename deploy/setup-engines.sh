#!/bin/sh
# One-time root setup of the Iris privilege split:
#
#   iris        admin account. Owns the engines directory and is the only
#               account that downloads and installs the official CLIs and
#               the voice models (through `iris engines`). No login shell.
#   iris-agent  the account Iris runs as (the "agent user"). It runs the
#               engines and every agent turn, but cannot change the engines,
#               cannot read anyone's home, and can ask the admin account for
#               exactly the `iris engines ...` commands, nothing else.
#
# Iris then runs as a system service (`systemctl status iris`) with its
# config and data in the agent account's home (/var/lib/iris-agent). After
# pairing, everything else (installing engines, signing in, updates) happens
# from the app. Without this script Iris still works in single-user mode.
#
# Usage (as root):
#   sh deploy/setup-engines.sh [options]
#     --agent-user NAME   run Iris as this existing account instead of
#                         creating iris-agent (keeps your systemd *user*
#                         service; see docs/SELF-HOSTING.md)
#     --iris-bin PATH     root-owned iris binary (default /usr/local/bin/iris)
#     --engines-dir PATH  engines directory (default /opt/iris/engines)
#     --copy-data DIR     copy an existing Iris data directory (for example
#                         /home/you/.local/share/iris) to the new agent
#                         account; CLI sign-ins are not copied, sign in again
#                         from the app
#     --no-start          install everything but do not start the service
#     --dry-run           print what would happen; write files only under
#                         $DESTDIR (required), run nothing as root
#   Legacy form: sh deploy/setup-engines.sh <agent-user> [iris-bin] [engines-dir]
#
# Afterwards, check everything with: sudo sh deploy/verify-split.sh
set -eu

ADMIN_USER="iris"
AGENT_USER=""
IRIS_BIN="/usr/local/bin/iris"
ENGINES_DIR="/opt/iris/engines"
COPY_DATA=""
START=1
DRY=0
HERE=$(cd "$(dirname "$0")" && pwd)

if [ $# -gt 0 ] && [ "${1#-}" = "$1" ]; then
    AGENT_USER="$1"
    IRIS_BIN="${2:-$IRIS_BIN}"
    ENGINES_DIR="${3:-$ENGINES_DIR}"
else
    while [ $# -gt 0 ]; do
        case "$1" in
            --agent-user) AGENT_USER="${2:?--agent-user needs a name}"; shift 2 ;;
            --iris-bin) IRIS_BIN="${2:?--iris-bin needs a path}"; shift 2 ;;
            --engines-dir) ENGINES_DIR="${2:?--engines-dir needs a path}"; shift 2 ;;
            --copy-data) COPY_DATA="${2:?--copy-data needs a directory}"; shift 2 ;;
            --no-start) START=0; shift ;;
            --dry-run) DRY=1; shift ;;
            -h|--help) sed -n '2,36p' "$0"; exit 0 ;;
            *) echo "unknown option: $1 (see --help)" >&2; exit 1 ;;
        esac
    done
fi

DEDICATED=0
if [ -z "$AGENT_USER" ]; then
    AGENT_USER="iris-agent"
    DEDICATED=1
fi
AGENT_HOME_DEFAULT="/var/lib/$AGENT_USER"
ROOT_PREFIX=""
if [ "$DRY" = 1 ]; then
    : "${DESTDIR:?--dry-run needs DESTDIR (a scratch directory to write files into)}"
    ROOT_PREFIX="$DESTDIR"
fi

say() { printf '%s\n' "$*"; }
# Run a command, or only print it in a dry run.
run() {
    if [ "$DRY" = 1 ]; then say "would run: $*"; else "$@"; fi
}

case "$IRIS_BIN$ENGINES_DIR$AGENT_USER$COPY_DATA" in
    *[!A-Za-z0-9/._-]*) echo "Names and paths may only contain letters, digits, / . _ -" >&2; exit 1 ;;
esac
case "$IRIS_BIN:$ENGINES_DIR" in
    /*:/*) ;;
    *) echo "--iris-bin and --engines-dir must be absolute paths" >&2; exit 1 ;;
esac
if [ "$DRY" = 0 ]; then
    if [ "$(id -u)" -ne 0 ]; then
        echo "Run this as root (for example with sudo), or try --dry-run." >&2
        exit 1
    fi
    if [ ! -x "$IRIS_BIN" ]; then
        echo "$IRIS_BIN is missing. Install it first, e.g.: install -Dm755 target/release/iris $IRIS_BIN" >&2
        exit 1
    fi
    if [ "$(stat -c %u "$IRIS_BIN")" != "0" ] || [ -n "$(find "$IRIS_BIN" -perm /022)" ]; then
        echo "$IRIS_BIN must be owned by root and not writable by others, so no one else can replace it." >&2
        exit 1
    fi
    if [ "$DEDICATED" = 0 ] && ! id "$AGENT_USER" >/dev/null 2>&1; then
        echo "No account named $AGENT_USER." >&2
        exit 1
    fi
fi

# 1. The admin account: no login shell, owns the engines.
if ! id "$ADMIN_USER" >/dev/null 2>&1; then
    run useradd --system --user-group --home-dir "$ENGINES_DIR" --no-create-home \
        --shell /usr/sbin/nologin "$ADMIN_USER"
fi

# 2. The agent account (dedicated mode): a system account whose home holds
#    Iris's config and data. Not in any admin group, no login shell.
if [ "$DEDICATED" = 1 ] && ! id "$AGENT_USER" >/dev/null 2>&1; then
    run useradd --system --user-group --home-dir "$AGENT_HOME_DEFAULT" --create-home \
        --shell /usr/sbin/nologin "$AGENT_USER"
fi
if [ "$DRY" = 1 ]; then
    AGENT_HOME="$AGENT_HOME_DEFAULT"
    [ "$DEDICATED" = 1 ] || AGENT_HOME="/home/$AGENT_USER"
else
    AGENT_HOME="$(getent passwd "$AGENT_USER" | cut -d: -f6)"
fi
if [ "$DEDICATED" = 1 ]; then
    run chmod 0700 "$AGENT_HOME"
fi

# 3. The engines directory: writable by the admin account only, readable
#    and executable by everyone (so the agent can run the engines).
run install -d -m 0755 -o "$ADMIN_USER" -g "$ADMIN_USER" "$ENGINES_DIR"

# 4. sudo rule: the agent may run exactly these commands as the admin
#    account (exact arguments; no wildcards, no shell, no other directory).
SUDOERS="$ROOT_PREFIX/etc/sudoers.d/iris-engines"
mkdir -p "$(dirname "$SUDOERS")"
{
    echo "# Installed by Iris deploy/setup-engines.sh. The agent account ($AGENT_USER)"
    echo "# may run only these exact commands as the engines admin ($ADMIN_USER)."
    printf 'Cmnd_Alias IRIS_ENGINES = %s engines list --dir %s' "$IRIS_BIN" "$ENGINES_DIR"
    for op in install update uninstall; do
        for name in claude codex gemini opencode tts stt; do
            printf ', \\\n    %s engines %s %s --dir %s' "$IRIS_BIN" "$op" "$name" "$ENGINES_DIR"
        done
    done
    echo ""
    echo "$AGENT_USER ALL=($ADMIN_USER) NOPASSWD: IRIS_ENGINES"
} > "$SUDOERS.tmp"
chmod 0440 "$SUDOERS.tmp"
if command -v visudo >/dev/null 2>&1; then
    if ! visudo -cf "$SUDOERS.tmp" >/dev/null; then
        rm -f "$SUDOERS.tmp"
        echo "The generated sudoers rule did not pass visudo; nothing was installed." >&2
        exit 1
    fi
fi
mv "$SUDOERS.tmp" "$SUDOERS"
say "sudo rule: $SUDOERS"

HELPER="[\"sudo\", \"-n\", \"-u\", \"$ADMIN_USER\", \"$IRIS_BIN\", \"engines\"]"

if [ "$DEDICATED" = 1 ]; then
    # 5a. Config for the agent account (only what differs from the
    #     defaults; `iris init` then creates the data directory).
    CONF_DIR="$ROOT_PREFIX$AGENT_HOME/.config/iris"
    CONF="$CONF_DIR/config.toml"
    if [ -e "$CONF" ]; then
        if ! grep -q "^helper = \[\"sudo\"" "$CONF"; then
            say "Kept the existing $CONF. Make sure its [engines] section has:"
            say "  dir = \"$ENGINES_DIR\""
            say "  helper = $HELPER"
        fi
    else
        mkdir -p "$CONF_DIR"
        cat > "$CONF" <<EOF
# Written by deploy/setup-engines.sh. Everything not set here uses the
# defaults; see templates/config.toml in the Iris repository for every
# option. Restart Iris after editing: sudo systemctl restart iris
bind = "127.0.0.1:7420"

[engines]
# Owned by the "$ADMIN_USER" account; Iris installs and updates engines
# through this helper and can only run them.
dir = "$ENGINES_DIR"
helper = $HELPER
EOF
        chmod 0600 "$CONF"
        run chown -R "$AGENT_USER:$AGENT_USER" "$AGENT_HOME/.config"
        chmod 0700 "$CONF_DIR"
        say "config: $CONF"
    fi
    # Bring over an existing data directory, if asked (never CLI sign-ins).
    DATA="$AGENT_HOME/.local/share/iris"
    if [ -n "$COPY_DATA" ]; then
        if [ -e "$ROOT_PREFIX$DATA" ] && [ -n "$(ls -A "$ROOT_PREFIX$DATA" 2>/dev/null)" ]; then
            echo "$DATA already exists and is not empty; not copying over it." >&2
            exit 1
        fi
        if [ ! -e "$COPY_DATA/iris.db" ]; then
            echo "$COPY_DATA does not look like an Iris data directory (no iris.db)." >&2
            exit 1
        fi
        run install -d -m 0700 -o "$AGENT_USER" -g "$AGENT_USER" "$AGENT_HOME/.local" "$AGENT_HOME/.local/share"
        run install -d -m 0700 "$DATA"
        # The contents, into the data directory itself.
        run cp -a "$COPY_DATA/." "$DATA/"
        if [ "$DRY" = 0 ] && [ ! -e "$DATA/iris.db" ]; then
            echo "Copying $COPY_DATA did not produce $DATA/iris.db; stopping." >&2
            exit 1
        fi
        # Engines the old setup installed for itself stay behind.
        run rm -rf "$DATA/engines"
        run chown -R "$AGENT_USER:$AGENT_USER" "$AGENT_HOME/.local"
    fi
    if [ "$DRY" = 1 ]; then
        say "would run: runuser -u $AGENT_USER -- env HOME=$AGENT_HOME $IRIS_BIN init"
    else
        runuser -u "$AGENT_USER" -- env HOME="$AGENT_HOME" "$IRIS_BIN" init >/dev/null
    fi

    # 6a. System service running as the agent account.
    UNIT="$ROOT_PREFIX/etc/systemd/system/iris.service"
    mkdir -p "$(dirname "$UNIT")"
    sed -e "s|@AGENT@|$AGENT_USER|g" -e "s|@AGENT_HOME@|$AGENT_HOME|g" \
        -e "s|@IRIS_BIN@|$IRIS_BIN|g" -e "s|@ENGINES_DIR@|$ENGINES_DIR|g" \
        "$HERE/iris-agent.service" > "$UNIT"
    chmod 0644 "$UNIT"
    say "service: $UNIT"
    run systemctl daemon-reload
    if [ "$START" = 1 ]; then
        run systemctl enable --now iris
    fi
    say ""
    say "Done. Iris runs as $AGENT_USER; engines are owned by $ADMIN_USER in $ENGINES_DIR."
    say "Pair your phone:  sudo runuser -u $AGENT_USER -- env HOME=$AGENT_HOME $IRIS_BIN pair"
    say "Logs:             journalctl -u iris -f"
    say "Check the split:  sudo sh $HERE/verify-split.sh"
    say "If Iris used to run as your own user, stop that one first:"
    say "  systemctl --user disable --now iris"
else
    # 5b/6b. Existing account with the bundled user unit: its
    #     NoNewPrivileges=yes blocks sudo, so relax only that.
    DROPIN_DIR="$ROOT_PREFIX$AGENT_HOME/.config/systemd/user/iris.service.d"
    mkdir -p "$DROPIN_DIR"
    cat > "$DROPIN_DIR/engines.conf" <<EOF
# Installed by Iris deploy/setup-engines.sh: allow 'sudo -u $ADMIN_USER iris engines'.
[Service]
NoNewPrivileges=no
EOF
    run chown "$AGENT_USER" "$AGENT_HOME/.config/systemd/user/iris.service.d" \
        "$AGENT_HOME/.config/systemd/user/iris.service.d/engines.conf"
    say "Done. Add to $AGENT_HOME/.config/iris/config.toml:"
    say ""
    say "  [engines]"
    say "  dir = \"$ENGINES_DIR\""
    say "  helper = $HELPER"
    say ""
    say "Then, as $AGENT_USER: systemctl --user daemon-reload && systemctl --user restart iris"
    say "Check the split: sudo sh $HERE/verify-split.sh --agent-user $AGENT_USER"
fi
say "Codex, Gemini CLI, and OpenCode need Node.js (npm) on the server; voice needs tar and bzip2."
