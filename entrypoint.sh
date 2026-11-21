#!/bin/bash
set -euo pipefail
umask 077

nft -f /etc/runner.nft
for address in ${RUNNER_HTTPS_ROUTE_IPS:-}; do
    nft insert rule inet runner output ip daddr "$address" tcp dport 443 accept
done
unset RUNNER_HTTPS_ROUTE_IPS
mkdir -p -m 0700 /runner/home /runner/actions
if mountpoint -q /cache; then
    mkdir -p /cache/cargo /cache/target
    mkdir -p /runner/home/.npm
    ln -s /cache /runner/home/.npm/_cacache
    chown gha:gha /runner/home/.npm
    chown gha:gha /cache/cargo /cache/target /cache
fi
chown gha:gha /runner/home /runner/actions
groups=(--clear-groups)
if [[ -n ${RUNNER_KVM_GID:-} ]]; then
    groups=(--groups "$RUNNER_KVM_GID")
    export NIX_CONFIG="${NIX_CONFIG:+$NIX_CONFIG$'\n'}extra-system-features = kvm nixos-test"
fi
unset RUNNER_KVM_GID
export RUNNER_MANUALLY_TRAP_SIG=1
as_runner=(setpriv --reuid=gha --regid=gha "${groups[@]}"
           --bounding-set=-all --inh-caps=-all --ambient-caps=-all --no-new-privs
           env --chdir=/runner/actions HOME=/runner/home)

IFS= read -r jit_config
test -n "$jit_config"
"${as_runner[@]}" cp -R /opt/actions-runner/. .
exec "${as_runner[@]}" ./run.sh --jitconfig "$jit_config" </dev/null
