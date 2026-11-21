#!/bin/sh
set -eu

# Fail before starting the engine or downloading the worker if configuration is incomplete.
gh-runner-pool --check
dockerd-entrypoint.sh dockerd --host="$DOCKER_HOST" --exec-opt native.cgroupdriver=cgroupfs \
    --feature containerd-snapshotter=false &
engine=$!
controller=
cleanup() {
    trap - EXIT TERM INT
    if [ -n "$controller" ]; then
        kill "$controller" 2>/dev/null || true
        wait "$controller" || true
    fi
    kill "$engine" 2>/dev/null || true
    wait "$engine" || true
}
trap cleanup EXIT
trap 'exit 0' TERM INT
until docker info >/dev/null 2>&1; do
    kill -0 "$engine" || exit 1
    sleep 1
done

worker_hash=$(cat /opt/worker.sha256)
if ! docker image inspect "gh-runner-worker:$worker_hash" >/dev/null 2>&1; then
    docker import --change 'ENTRYPOINT ["/usr/local/bin/runner-entrypoint"]' \
        --change 'VOLUME ["/nix"]' /opt/worker.tar.gz "gh-runner-worker:$worker_hash"
fi
docker tag "gh-runner-worker:$worker_hash" gh-runner-worker:local
gh-runner-pool &
controller=$!
# Restart the Compose service if either the controller or its engine exits.
while kill -0 "$controller" && kill -0 "$engine"; do sleep 1; done
exit 1
