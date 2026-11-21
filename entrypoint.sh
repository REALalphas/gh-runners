#!/bin/bash
set -euo pipefail
umask 077

nft -f /etc/runner.nft
mkdir -p -m 0700 /runner/home /runner/actions
chown gha:gha /runner/home /runner/actions
as_runner=(setpriv --reuid=gha --regid=gha --clear-groups
           --bounding-set=-all --inh-caps=-all --ambient-caps=-all --no-new-privs
           env --chdir=/runner/actions HOME=/runner/home)

IFS= read -r jit_config
test -n "$jit_config"
"${as_runner[@]}" cp -R /opt/actions-runner/. .
exec "${as_runner[@]}" ./run.sh --jitconfig "$jit_config" </dev/null
