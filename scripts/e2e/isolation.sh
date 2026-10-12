#!/bin/sh
# After root setup, run as the Iris service user (or the dedicated agent).
# Uses only the exact sudoers launcher; no root-only commands or file changes.
# Usage: isolation.sh AGENT_USER [DATA_DIR] [IRIS_BIN]
set -eu
AGENT_USER=${1:?Usage: isolation.sh AGENT_USER [DATA_DIR] [IRIS_BIN]}
DATA_DIR=${2:-}
IRIS_BIN=${3:-/usr/local/bin/iris}
python3 - "$AGENT_USER" "$DATA_DIR" "$IRIS_BIN" <<'PY'
import json, os, struct, subprocess, sys
user, data, binary = sys.argv[1:]
with open('/etc/iris-agent.json') as f: p = json.load(f)
assert p['user'] == user and p['launcher'] == binary, 'arguments must match root setup'
assert not data or p['data_dir'] == data, 'data directory must match root setup'
if os.geteuid() == p['service_uid']:
    for name in ['vault.key', 'iris.db']: assert os.path.isfile(p['data_dir']+'/'+name), 'initialize Iris first'
r = dict(op='check', scope='probe', harness='', args=[], env=[], path='')
b = json.dumps(r).encode()
cmd = [binary, 'agent-exec'] if os.geteuid() == p['uid'] else ['sudo', '-n', '-u', user, '--', binary, 'agent-exec']
out = subprocess.run(cmd, input=struct.pack('<Q', len(b))+b, stdout=subprocess.PIPE, check=True)
report = json.loads(out.stdout)
assert report['isolated'] is True and report['uid'] == p['uid']
print('PASS: as the agent user, opening vault.key, iris.db and config.toml returns PermissionDenied; launcher works.')
PY
