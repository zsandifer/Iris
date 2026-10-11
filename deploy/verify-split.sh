#!/bin/sh
# Check the Iris privilege split made by deploy/setup-engines.sh. Read-only
# except for two throwaway probe files it tries (and is expected to fail) to
# create. Run once as root:
#
#   sudo sh deploy/verify-split.sh [--agent-user NAME] [--iris-bin PATH]
#                                  [--engines-dir PATH] [--url URL]
#
# Defaults match setup-engines.sh: agent iris-agent, admin iris,
# /usr/local/bin/iris, /opt/iris/engines, and http://127.0.0.1:7420.
# Prints one line per check and exits non-zero if any check fails.
set -u

ADMIN_USER="iris"
AGENT_USER="iris-agent"
IRIS_BIN="/usr/local/bin/iris"
ENGINES_DIR="/opt/iris/engines"
URL="http://127.0.0.1:7420"
while [ $# -gt 0 ]; do
    case "$1" in
        --agent-user) AGENT_USER="$2"; shift 2 ;;
        --iris-bin) IRIS_BIN="$2"; shift 2 ;;
        --engines-dir) ENGINES_DIR="$2"; shift 2 ;;
        --url) URL="$2"; shift 2 ;;
        -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
        *) echo "unknown option: $1" >&2; exit 2 ;;
    esac
done
if [ "$(id -u)" -ne 0 ]; then
    echo "Run this as root: sudo sh $0" >&2
    exit 2
fi

PASS=0
FAIL=0
WARN=0
pass() { PASS=$((PASS + 1)); printf 'PASS  %s\n' "$*"; }
fail() { FAIL=$((FAIL + 1)); printf 'FAIL  %s\n' "$*"; }
warn() { WARN=$((WARN + 1)); printf 'WARN  %s\n' "$*"; }
# Run as the agent account, the way the service does (no tty, own HOME).
as_agent() { runuser -u "$AGENT_USER" -- env HOME="$AGENT_HOME" PATH=/usr/local/bin:/usr/bin:/bin "$@"; }

echo "Iris privilege split: admin=$ADMIN_USER agent=$AGENT_USER engines=$ENGINES_DIR"
echo

# --- accounts
if id "$ADMIN_USER" >/dev/null 2>&1; then pass "admin account $ADMIN_USER exists"; else fail "admin account $ADMIN_USER is missing"; fi
if id "$AGENT_USER" >/dev/null 2>&1; then pass "agent account $AGENT_USER exists"; else fail "agent account $AGENT_USER is missing"; echo; echo "$FAIL failed"; exit 1; fi
AGENT_HOME=$(getent passwd "$AGENT_USER" | cut -d: -f6)
case "$(getent passwd "$ADMIN_USER" | cut -d: -f7)" in
    */nologin|*/false) pass "$ADMIN_USER has no login shell" ;;
    *) fail "$ADMIN_USER has a login shell" ;;
esac
if [ "$AGENT_USER" = "iris-agent" ]; then
    case "$(getent passwd "$AGENT_USER" | cut -d: -f7)" in
        */nologin|*/false) pass "$AGENT_USER has no login shell" ;;
        *) fail "$AGENT_USER has a login shell" ;;
    esac
fi
GROUPS_OF_AGENT=$(id -nG "$AGENT_USER")
case " $GROUPS_OF_AGENT " in
    *" sudo "*|*" wheel "*|*" admin "*|*" root "*|*" $ADMIN_USER "*) fail "$AGENT_USER is in a privileged group ($GROUPS_OF_AGENT)" ;;
    *) pass "$AGENT_USER is in no admin group ($GROUPS_OF_AGENT)" ;;
esac

# --- binary and engines directory
if [ -x "$IRIS_BIN" ] && [ "$(stat -c %u "$IRIS_BIN")" = 0 ] && [ -z "$(find "$IRIS_BIN" -perm /022)" ]; then
    pass "$IRIS_BIN is root-owned and not writable by others"
else
    fail "$IRIS_BIN must exist, be owned by root, and not be group/other writable"
fi
if [ -d "$ENGINES_DIR" ] && [ "$(stat -c %U "$ENGINES_DIR")" = "$ADMIN_USER" ]; then
    pass "$ENGINES_DIR is owned by $ADMIN_USER"
else
    fail "$ENGINES_DIR is missing or not owned by $ADMIN_USER"
fi
WRITABLE=$(find "$ENGINES_DIR" ! -type l -perm /022 2>/dev/null | head -3)
if [ -z "$WRITABLE" ]; then pass "nothing in $ENGINES_DIR is group/other writable"; else fail "group/other writable in $ENGINES_DIR: $WRITABLE"; fi
NOT_ADMIN=$(find "$ENGINES_DIR" ! -user "$ADMIN_USER" 2>/dev/null | head -3)
if [ -z "$NOT_ADMIN" ]; then pass "everything in $ENGINES_DIR belongs to $ADMIN_USER"; else fail "not owned by $ADMIN_USER: $NOT_ADMIN"; fi
PROBE="$ENGINES_DIR/.agent-write-probe-$$"
if as_agent sh -c "touch '$PROBE'" 2>/dev/null; then fail "$AGENT_USER can write into $ENGINES_DIR"; rm -f "$PROBE"; else pass "$AGENT_USER cannot write into $ENGINES_DIR"; fi
if as_agent test -w "$IRIS_BIN"; then fail "$AGENT_USER can modify $IRIS_BIN"; else pass "$AGENT_USER cannot modify $IRIS_BIN"; fi
if [ -d "$ENGINES_DIR/bin" ]; then
    if as_agent sh -c "ls '$ENGINES_DIR/bin' >/dev/null"; then pass "$AGENT_USER can list and run installed engines"; else fail "$AGENT_USER cannot read $ENGINES_DIR/bin"; fi
else
    warn "no engines installed yet ($ENGINES_DIR/bin missing); install one from the app, then re-run"
fi

# --- sudo rule
SUDOERS=/etc/sudoers.d/iris-engines
if [ -f "$SUDOERS" ] && visudo -cf "$SUDOERS" >/dev/null 2>&1; then pass "$SUDOERS parses (visudo)"; else fail "$SUDOERS is missing or invalid"; fi
if [ "$(stat -c %a "$SUDOERS" 2>/dev/null)" = 440 ] && [ "$(stat -c %u "$SUDOERS" 2>/dev/null)" = 0 ]; then pass "$SUDOERS is root-owned, mode 0440"; else fail "$SUDOERS must be root-owned with mode 0440"; fi
LIST=$(sudo -l -U "$AGENT_USER" 2>/dev/null | tr -s " \n\t" "   ")
if printf '%s\n' "$LIST" | grep -Eq '\((ALL|root)[^)]*\)'; then
    fail "$AGENT_USER may run commands as root or ALL: check sudo -l -U $AGENT_USER"
else
    pass "$AGENT_USER may not run anything as root"
fi
if printf '%s\n' "$LIST" | grep -Eq "$IRIS_BIN engines install claude --dir $ENGINES_DIR|IRIS_ENGINES"; then
    pass "$AGENT_USER may run the 'iris engines' commands as $ADMIN_USER"
else
    fail "sudo -l -U $AGENT_USER does not show the iris engines commands"
fi
OUT=$(as_agent sudo -n -u "$ADMIN_USER" "$IRIS_BIN" engines list --dir "$ENGINES_DIR" 2>&1)
case "$OUT" in
    \[*) pass "the helper works: $AGENT_USER -> sudo -u $ADMIN_USER iris engines list" ;;
    *) fail "the helper failed: $OUT" ;;
esac
if as_agent sudo -n -u "$ADMIN_USER" /bin/true 2>/dev/null; then fail "$AGENT_USER can run arbitrary commands as $ADMIN_USER"; else pass "$AGENT_USER cannot run other commands as $ADMIN_USER"; fi
# Every allowed command must be an exact `iris engines ... --dir <engines dir>`
# (checked from the rule list, without running anything).
CMDS=$(sed -n '/^Cmnd_Alias IRIS_ENGINES/,/[^\\]$/p' "$SUDOERS" | sed 's/^Cmnd_Alias IRIS_ENGINES = //; s/\\$//' \
    | tr ',' '\n' | sed 's/^ *//; s/ *$//' | grep -v '^$')
if grep -v '^#' "$SUDOERS" | grep -v '^Cmnd_Alias' | grep -v '^ ' | grep -v '^$' | grep -qv "^$AGENT_USER ALL=($ADMIN_USER) NOPASSWD: IRIS_ENGINES\$"; then
    fail "$SUDOERS has rules other than the IRIS_ENGINES alias for $AGENT_USER"
fi
BAD=$(printf '%s\n' "$CMDS" | grep -v "^$IRIS_BIN engines [a-z]* *[a-z]* *--dir $ENGINES_DIR\$" | head -3)
if [ -n "$CMDS" ] && [ -z "$BAD" ]; then
    pass "every command $AGENT_USER may run as $ADMIN_USER is an exact 'iris engines ... --dir $ENGINES_DIR'"
else
    fail "unexpected sudo rule for $AGENT_USER: ${BAD:-none found}"
fi
if as_agent sudo -n true 2>/dev/null; then fail "$AGENT_USER can run commands as root"; else pass "$AGENT_USER cannot sudo to root"; fi

# --- what the agent can see
HUMAN=${SUDO_USER:-}
if [ -n "$HUMAN" ] && [ "$HUMAN" != "$AGENT_USER" ] && [ "$HUMAN" != root ]; then
    HUMAN_HOME=$(getent passwd "$HUMAN" | cut -d: -f6)
    if as_agent ls "$HUMAN_HOME" >/dev/null 2>&1; then
        warn "$AGENT_USER can list $HUMAN_HOME outside the service (chmod 750 $HUMAN_HOME to close it; the service itself runs with ProtectHome=yes)"
    else
        pass "$AGENT_USER cannot read $HUMAN_HOME"
    fi
fi
if [ "$(stat -c %a "$AGENT_HOME")" = 700 ]; then pass "$AGENT_HOME is private (0700)"; else warn "$AGENT_HOME is mode $(stat -c %a "$AGENT_HOME"); 0700 keeps Iris data private"; fi
if runuser -u "$ADMIN_USER" -- ls "$AGENT_HOME" >/dev/null 2>&1; then fail "$ADMIN_USER can read $AGENT_HOME"; else pass "$ADMIN_USER cannot read Iris's data in $AGENT_HOME"; fi

# --- config and service
CONF="$AGENT_HOME/.config/iris/config.toml"
if [ -f "$CONF" ] && grep -q "^helper = \[\"sudo\", \"-n\", \"-u\", \"$ADMIN_USER\"" "$CONF" && grep -q "^dir = \"$ENGINES_DIR\"" "$CONF"; then
    pass "$CONF uses the helper and $ENGINES_DIR"
else
    fail "$CONF must set [engines] dir = \"$ENGINES_DIR\" and the sudo helper"
fi
if [ "$AGENT_USER" = "iris-agent" ] || systemctl cat iris >/dev/null 2>&1; then
    if [ "$(systemctl show iris -p User --value 2>/dev/null)" = "$AGENT_USER" ]; then pass "iris.service runs as $AGENT_USER"; else fail "iris.service does not run as $AGENT_USER"; fi
    if [ "$(systemctl show iris -p ProtectHome --value 2>/dev/null)" = yes ]; then pass "iris.service hides /home (ProtectHome=yes)"; else warn "iris.service does not set ProtectHome=yes"; fi
    if [ "$(systemctl show iris -p NoNewPrivileges --value 2>/dev/null)" = no ]; then pass "iris.service allows sudo for the helper (NoNewPrivileges=no)"; else fail "iris.service sets NoNewPrivileges, which blocks the engine helper"; fi
    if systemctl is-active --quiet iris; then
        pass "iris.service is running"
        PID=$(systemctl show iris -p MainPID --value)
        if [ "$(stat -c %U "/proc/$PID")" = "$AGENT_USER" ]; then pass "the running process belongs to $AGENT_USER (pid $PID)"; else fail "the running process does not belong to $AGENT_USER"; fi
        # The helper must also work inside the service's own sandbox: run it
        # in a transient unit with every sandboxing property the service has.
        PROPS=""
        for p in User ProtectSystem ProtectHome PrivateTmp PrivateDevices ProtectKernelTunables \
            ProtectKernelModules ProtectKernelLogs ProtectClock ProtectHostname ProtectControlGroups \
            RestrictRealtime RestrictNamespaces LockPersonality MemoryDenyWriteExecute \
            NoNewPrivileges RestrictSUIDSGID SystemCallArchitectures; do
            v=$(systemctl show iris -p "$p" --value 2>/dev/null)
            [ -n "$v" ] && PROPS="$PROPS -p $p=$v"
        done
        RW=$(systemctl show iris -p ReadWritePaths --value 2>/dev/null)
        FILTER=$(systemctl show iris -p SystemCallFilter --value 2>/dev/null)
        [ -n "$FILTER" ] && warn "iris.service has a SystemCallFilter; it can block sudo for the engine helper"
        # shellcheck disable=SC2086
        if systemd-run --wait --quiet --collect --pipe $PROPS -p ReadWritePaths="$RW" \
            sudo -n -u "$ADMIN_USER" "$IRIS_BIN" engines list --dir "$ENGINES_DIR" 2>/dev/null | grep -q '^\['; then
            pass "the helper works inside the service's sandbox"
        else
            fail "the helper fails inside the service's sandbox (check NoNewPrivileges and seccomp options)"
        fi
    else
        fail "iris.service is not running: journalctl -u iris -n 50"
    fi
fi
if command -v curl >/dev/null 2>&1; then
    if curl -sf "$URL/api/health" | grep -q '"ok":true'; then pass "Iris answers at $URL"; else fail "no answer from $URL/api/health"; fi
fi

echo
echo "$PASS passed, $FAIL failed, $WARN warnings"
[ "$FAIL" -eq 0 ]
