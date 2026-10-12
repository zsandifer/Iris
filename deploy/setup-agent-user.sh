#!/bin/sh
# Run once as root, after setup-engines.sh. Never gives the agent sudo rights.
# Usage: [--check|--uninstall] SERVICE_USER [AGENT_USER] [IRIS_BIN] [DATA_DIR] [CONFIG_FILE] [ENGINES_DIR]
set -eu
render_sudoers() {
    for name in "$1" "$2"; do case "$name" in ''|*[!A-Za-z0-9_-]*) echo 'Invalid account name' >&2; return 1;; esac; done
    case "$3" in /*) ;; *) echo 'Launcher must be absolute' >&2; return 1;; esac
    case "$3" in *[!A-Za-z0-9/._-]*) echo 'Invalid launcher path' >&2; return 1;; esac
    printf 'Defaults:%s env_reset\n%s ALL=(%s) NOPASSWD: NOSETENV: %s agent-exec\n' "$1" "$1" "$2" "$3"
}
if [ "${1:-}" = --render-sudoers ]; then
    [ "$#" = 4 ] || { echo 'Usage: --render-sudoers SERVICE_USER AGENT_USER IRIS_BIN' >&2; exit 1; }
    render_sudoers "$2" "$3" "$4"
    exit
fi
MODE=install
case "${1:-}" in --check) MODE=check; shift;; --uninstall) MODE=uninstall; shift;; esac
SERVICE_USER=${1:?Usage: setup-agent-user.sh [--check|--uninstall] SERVICE_USER [AGENT_USER] [IRIS_BIN] [DATA_DIR] [CONFIG_FILE] [ENGINES_DIR]}
AGENT_USER=${2:-iris-agent}
IRIS_BIN=${3:-/usr/local/bin/iris}
SERVICE_HOME=$(getent passwd "$SERVICE_USER" | cut -d: -f6)
DATA_DIR=${4:-$SERVICE_HOME/.local/share/iris}
CONFIG_FILE=${5:-$SERVICE_HOME/.config/iris/config.toml}
ENGINES_DIR=${6:-/opt/iris/engines}
BASE=/var/lib/iris-agent
POLICY=/etc/iris-agent.json
SUDOERS=/etc/sudoers.d/iris-agent
for word in "$SERVICE_USER" "$AGENT_USER" "$IRIS_BIN" "$DATA_DIR" "$CONFIG_FILE" "$ENGINES_DIR"; do
    case "$word" in ''|*[!A-Za-z0-9/._-]*) echo 'User names and paths must use only letters, digits, / . _ -' >&2; exit 1;; esac
done
case "$SERVICE_USER$AGENT_USER" in *[!A-Za-z0-9_-]*) echo 'Invalid user name' >&2; exit 1;; esac
[ "$SERVICE_USER" != "$AGENT_USER" ] || { echo 'Service and agent users must differ' >&2; exit 1; }
[ -n "$SERVICE_HOME" ] && [ -d "$DATA_DIR" ] && [ -f "$CONFIG_FILE" ] || { echo 'Run iris init as the service user first' >&2; exit 1; }

# Check mode runs as the service user after root setup and changes nothing.
if [ "$MODE" = check ]; then
    [ "$(id -u)" = "$(id -u "$SERVICE_USER")" ] || { echo '--check must run as the service user' >&2; exit 1; }
    python3 - "$IRIS_BIN" "$AGENT_USER" <<'PY'
import json, struct, subprocess, sys
binary, user = sys.argv[1:]
r = dict(op="check", scope="probe", harness="", args=[], env=[], path="")
b = json.dumps(r).encode()
subprocess.run(["sudo", "-n", "-u", user, "--", binary, "agent-exec"], input=struct.pack("<Q", len(b))+b, check=True)
PY
    # The launcher also checks these itself, without opening their contents.
    sh "$(dirname "$0")/../scripts/e2e/isolation.sh" "$AGENT_USER" "$DATA_DIR" "$IRIS_BIN"
    echo 'Verified: launcher works and the agent cannot read Iris data.'
    exit 0
fi
[ "$(id -u)" = 0 ] || { echo 'Run setup/rollback as root; --check runs as the service user.' >&2; exit 1; }
command -v visudo >/dev/null || { echo 'Install sudo (including visudo) first.' >&2; exit 1; }
command -v python3 >/dev/null || { echo 'Install Python 3 for setup checks first.' >&2; exit 1; }
command -v runuser >/dev/null || { echo 'Install util-linux (runuser) first.' >&2; exit 1; }
require_root_chain() {
    p=$1
    [ "$(readlink -f "$p")" = "$p" ] || { echo "$p must be a real absolute path" >&2; exit 1; }
    while :; do
        [ "$(stat -c %u "$p")" = 0 ] && [ $((0$(stat -c %a "$p") & 022)) = 0 ] || { echo "$p must be root-owned and not writable by group/others" >&2; exit 1; }
        [ "$p" = / ] && break
        p=$(dirname "$p")
    done
}
require_root_chain "$IRIS_BIN"
require_root_chain /etc
require_root_chain /etc/sudoers.d
if [ "$MODE" = uninstall ]; then
    rm -f "$SUDOERS" "$POLICY"
    runuser -u "$SERVICE_USER" -- rm -f "$SERVICE_HOME/.config/systemd/user/iris.service.d/agent.conf"
    echo 'Removed /etc/sudoers.d/iris-agent, /etc/iris-agent.json and agent.conf.'
    echo 'Set [agent] user = "" and restart Iris. Agent home and workspaces are retained.'
    echo 'Existing sessions stay with the agent account; sign in again as the service user.'
    exit 0
fi
[ "$(id -u "$SERVICE_USER")" != 0 ] || { echo 'The service user must not be root' >&2; exit 1; }
# Refuse existing trees the agent could have swapped or changed.
for dir in "$BASE" "$BASE/workspace" "$BASE/turns"; do
    if [ -e "$dir" ] || [ -L "$dir" ]; then
        [ -d "$dir" ] && [ ! -L "$dir" ] || { echo "Refusing $dir" >&2; exit 1; }
        case "$(stat -c %U "$dir")" in root|"$SERVICE_USER") ;; *) echo "Untrusted owner of $dir" >&2; exit 1;; esac
    fi
done
require_root_chain /var/lib
install -d -m 0755 -o root -g root "$BASE"
if ! getent group "$AGENT_USER" >/dev/null; then groupadd --system "$AGENT_USER"; echo "Created group $AGENT_USER"; fi
if ! id "$AGENT_USER" >/dev/null 2>&1; then
    useradd --system --gid "$AGENT_USER" --home-dir "$BASE/home" --create-home --shell /usr/sbin/nologin "$AGENT_USER"
    echo "Created user $AGENT_USER with home $BASE/home"
fi
[ "$(id -u "$AGENT_USER")" != 0 ] && [ "$(id -g "$AGENT_USER")" = "$(getent group "$AGENT_USER" | cut -d: -f3)" ] || { echo 'Agent must be non-root with its dedicated primary group' >&2; exit 1; }
[ "$(getent passwd "$AGENT_USER" | cut -d: -f6)" = "$BASE/home" ] || { echo "Existing agent home must be $BASE/home; use a fresh dedicated account" >&2; exit 1; }
[ ! -L "$BASE/home" ] || { echo "Agent home cannot be a symlink" >&2; exit 1; }
install -d -m 0700 -o "$AGENT_USER" -g "$AGENT_USER" "$BASE/home"
install -d -m 0730 -o "$SERVICE_USER" -g "$AGENT_USER" "$BASE/workspace"
install -d -m 0750 -o "$SERVICE_USER" -g "$AGENT_USER" "$BASE/turns"
echo "Provisioned $BASE/home (0700), workspace parent (0730), turns (0750)."
usermod -a -G "$AGENT_USER" "$SERVICE_USER"
echo "Added $SERVICE_USER to $AGENT_USER for per-turn file sharing; restart the user manager."
# Do not follow symlinks; retain service ownership throughout private data.
[ "$(readlink -f "$DATA_DIR")" = "$DATA_DIR" ] && [ "$(readlink -f "$CONFIG_FILE")" = "$CONFIG_FILE" ] || { echo 'Data/config paths must be real absolute paths' >&2; exit 1; }
chown -h "$SERVICE_USER:$(id -gn "$SERVICE_USER")" "$DATA_DIR" "$CONFIG_FILE"
runuser -u "$SERVICE_USER" -- find -P "$DATA_DIR" -xdev -type d -exec chmod 0700 {} +
runuser -u "$SERVICE_USER" -- find -P "$DATA_DIR" -xdev -type f -exec chmod 0600 {} +
runuser -u "$SERVICE_USER" -- chmod 0600 "$CONFIG_FILE"
runuser -u "$SERVICE_USER" -- chmod 0700 "$(dirname "$CONFIG_FILE")"
echo "Made $DATA_DIR directories 0700/files 0600 and $CONFIG_FILE 0600."
# Fixed sudo command, no arguments, no SETENV. Framed request goes over stdin.
tmp=$(mktemp /etc/sudoers.d/.iris-agent.XXXXXX)
trap 'rm -f "$tmp"' EXIT HUP INT TERM
render_sudoers "$SERVICE_USER" "$AGENT_USER" "$IRIS_BIN" > "$tmp"
chmod 0440 "$tmp"
visudo -cf "$tmp"
mv "$tmp" "$SUDOERS"
trap - EXIT HUP INT TERM
echo "Installed and validated $SUDOERS (only $IRIS_BIN agent-exec as $AGENT_USER)."
# Atomic root-owned policy; managed engines may not yet be installed.
python3 - "$POLICY" "$SERVICE_USER" "$AGENT_USER" "$BASE" "$DATA_DIR" "$CONFIG_FILE" "$IRIS_BIN" "$ENGINES_DIR" <<'PY'
import json, os, pwd, sys, tempfile
out, service, agent, base, data, config, launcher, engines = sys.argv[1:]
s, a = pwd.getpwnam(service), pwd.getpwnam(agent)
p = dict(user=agent, uid=a.pw_uid, gid=a.pw_gid, service_uid=s.pw_uid, home=base+"/home", workspace=base+"/workspace", runtime=base+"/turns", data_dir=data, config_file=config, launcher=launcher, engines={name:engines+"/bin/"+name for name in ["claude", "codex", "gemini", "opencode"]})
fd, tmp = tempfile.mkstemp(dir="/etc", prefix=".iris-agent.")
with os.fdopen(fd, "w") as f: json.dump(p, f); f.write("\n")
os.chmod(tmp, 0o644)
os.replace(tmp, out)
PY
echo "Installed $POLICY (root-owned, 0644; contains paths and ids only)."
runuser -u "$SERVICE_USER" -- sh -c 'umask 077; mkdir -p "$1"; printf "[Service]\nNoNewPrivileges=no\n" > "$1/agent.conf"' sh "$SERVICE_HOME/.config/systemd/user/iris.service.d"
echo 'Installed agent.conf (NoNewPrivileges=no for the narrowly scoped sudo launcher).'
# Pre-create the probe folder so --check is strictly read-only.
runuser -u "$AGENT_USER" -- sh -c 'umask 077; mkdir -p "$1/probe"' sh "$BASE/workspace"
echo "Created $BASE/workspace/probe as $AGENT_USER (0700)."
echo "Set [agent] user = \"$AGENT_USER\", launcher = \"$IRIS_BIN\" in $CONFIG_FILE."
echo 'Stop Iris, restart the service user manager to pick up group membership, then restart Iris.'
echo "Sign each CLI in as $AGENT_USER through the app (or runuser -u $AGENT_USER -- env HOME=$BASE/home <CLI> login as root)."
echo "As $SERVICE_USER: sh $0 --check $SERVICE_USER $AGENT_USER $IRIS_BIN $DATA_DIR $CONFIG_FILE $ENGINES_DIR"
