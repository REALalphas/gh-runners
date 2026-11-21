#!/bin/bash
set -euo pipefail
umask 077

nft -f /etc/runner.nft
install -d -m 0700 -o gha -g gha /runner/home /runner/actions
as_runner=(setpriv --reuid=gha --regid=gha --clear-groups
           --bounding-set=-all --inh-caps=-all --ambient-caps=-all --no-new-privs
           env HOME=/runner/home --chdir=/runner/actions)

IFS= read -r jit_config
test -n "$jit_config"
"${as_runner[@]}" cp -R /opt/actions-runner/. .
exec "${as_runner[@]}" ./run.sh --jitconfig "$jit_config" </dev/null
