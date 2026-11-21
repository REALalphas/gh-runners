#!/bin/bash
set -euo pipefail
umask 077

nft -f /etc/runner.nft
for address in ${RUNNER_HTTPS_ROUTE_IPS:-}; do
    nft insert rule inet runner output ip daddr "$address" tcp dport 443 accept
done
unset RUNNER_HTTPS_ROUTE_IPS
mkdir -p -m 0700 /runner/home /runner/tools
if mountpoint -q /cache; then
    chown gha:gha /cache
fi
chown gha:gha /runner/home /runner/tools
groups=(--clear-groups)
if [[ -n ${RUNNER_KVM_GID:-} ]]; then
    groups=(--groups "$RUNNER_KVM_GID")
    export NIX_CONFIG="${NIX_CONFIG:+$NIX_CONFIG$'\n'}extra-system-features = kvm nixos-test"
fi
unset RUNNER_KVM_GID
export RUNNER_MANUALLY_TRAP_SIG=1
# Keep the workspace under HOME at the same depth as on GitHub-hosted runners.
as_runner=(setpriv --reuid=gha --regid=gha "${groups[@]}"
           --bounding-set=-all --inh-caps=-all --ambient-caps=-all --no-new-privs
           env --chdir=/runner/home HOME=/runner/home
           RUNNER_TOOL_CACHE=/runner/tools PATH=/runner/tools/node/26.13.1/x64/bin:/runner/tools/node/24.22.3/x64/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin)

"${as_runner[@]}" bash -eu -c '
    for version in /opt/hostedtoolcache/node/*; do
        [[ -f "$version/x64.complete" ]] || continue
        path="$RUNNER_TOOL_CACHE/node/${version##*/}"
        mkdir -p "$path"
        ln -s "$version"/x64 "$version"/x64.complete "$path/"
    done
    if mountpoint -q /cache; then
        mkdir -p "$HOME/.npm"
        ln -s /cache "$HOME/.npm/_cacache"
        if [[ -n ${CARGO_BUILD_JOBS:-} ]]; then
            mkdir -p "${CARGO_HOME:-$HOME/.cargo}/registry" /cache/cargo-registry
            ln -s /cache/cargo-registry "${CARGO_HOME:-$HOME/.cargo}/registry/cache"
        fi
        if [[ -n ${CARGO_TARGET_DIR:-} ]]; then
            mkdir -p "$CARGO_TARGET_DIR"
        fi
    fi
'

IFS= read -r jit_config
test -n "$jit_config"
"${as_runner[@]}" cp -R /opt/actions-runner/. .
exec "${as_runner[@]}" ./run.sh --jitconfig "$jit_config" </dev/null
