# Runners Controller

![Runners Controller — ephemeral GitHub Actions runners](assets/banner.png)

Runners Controller starts a temporary Linux x64 container for each GitHub Actions job. The container and its workspace are removed when the job finishes. The image includes Node.js, Nix, and Cachix.

## Requirements

- Linux x86-64 with cgroup v2
- Docker Engine
- Docker Compose v2

The host should have at least as many logical CPUs as the `cpus` limit in `compose.yaml`.

## Setup

### 1. Create a GitHub App

In the app settings:

- disable the webhook;
- grant **Actions — Read-only** and **Administration — Read and write** repository permissions;
- install the app in the repositories that should use the pool.

Create a private key with **Generate a private key** and save it in the repository:

```sh
install -d -m 700 .secrets
install -m 600 ~/Downloads/your-app.pem .secrets/github-app.pem
```

### 2. Configure `compose.yaml`

Set these values:

- `GITHUB_APP_ID` — the app ID;
- `GITHUB_INSTALLATION_ID` — the installation ID from the **Configure** page URL;
- `GITHUB_REPOSITORIES` — comma-separated repositories such as `owner/api,owner/web`. Leave it empty to use every repository in the installation;
- `cpus` and `mem_limit` — the total CPU and memory available to the pool. The example uses 16 CPUs and 32 GiB of memory.

### 3. Start the pool

```sh
docker compose up -d --build
docker compose logs -f pool
```

The `Polling GitHub` message means that the controller is running and checking for new jobs.

## Runner profiles

Set the profile with `runs-on`.

| Profile | CPU | Memory |
| --- | ---: | ---: |
| `tiny` | 1 | 2 GiB |
| `small` | 2 | 4 GiB |
| `medium` | 4 | 8 GiB |
| `large` | 8 | 16 GiB |
| `huge` | 16 | 32 GiB |
| `tiny-compute` | 1 | 1 GiB |
| `small-compute` | 2 | 2 GiB |
| `medium-compute` | 4 | 4 GiB |
| `large-compute` | 8 | 8 GiB |
| `huge-compute` | 16 | 16 GiB |

Profiles ending in `-compute` provide the same CPU with less memory. Use them for jobs that are more CPU-bound than memory-bound.

Example:

```yaml
name: Pool smoke test
on: workflow_dispatch
jobs:
  smoke:
    runs-on: [self-hosted, gh-runner-pool, small-compute]
    steps:
      - run: id
```

The controller accepts the additional `Linux`, `X64`, and `gh-runner-pool` labels. A job with only `runs-on: self-hosted` uses the `tiny` profile. A profile-only label can be claimed by another self-hosted runner, so use `gh-runner-pool` when this pool must receive the job.

Include `gh-runner-pool` when a job should use this pool and other self-hosted runners use the same profile labels. The manual **Runner smoke test** selects this label and checks the runner name and profile for each job.

## Polling

Each pass samples local workers and checks GitHub for repositories with a local worker or eligible queued jobs. Repositories without either condition are checked after at least `max(60, 3 × repository count)` seconds, on the next pass. On startup and about every 10 minutes the controller refreshes the repository list and checks all watched repositories, including their runner registrations.

## Resource limits

- `cpus` and `mem_limit` set the total resources available to all jobs and the Docker Engine inside the service.
- A job needs all CPUs assigned to its profile before it can start.
- A worker receives up to the profile's memory limit, capped by the remaining pool budget. Starting a worker requires at least 6 MiB of free pool memory.
- Leave some headroom for memory spikes. The kernel may stop a process when the memory limit is reached.

## Update and stop

Wait for active jobs to finish before updating the service.

```sh
docker compose up -d --build  # update
docker compose down           # stop
```

The `engine` volume keeps the runner image between restarts.

## Optional settings

Set these variables under `services.pool.environment`.

| Variable | Purpose |
| --- | --- |
| `POLL_SECONDS` | Delay after a successful controller pass. Defaults to 20 seconds. |
| `CACHE_ROOT` | Absolute directory for npm and Cargo caches. Mount a persistent volume there. |
| `CARGO_TARGET_CACHE_REPOSITORIES` | Comma-separated `owner/repo` list for reusing Cargo target directories. Requires `CACHE_ROOT`. |
| `CACHE_PRIVATE_CARGO_TARGETS` | Set to `true` to cache Cargo targets for private repositories. Use only when fork jobs are disabled. |
| `NIX_BINARY_CACHE` | Directory containing a signed Nix cache and its `nix-cache-info` file. Mounted read-only. |
| `NIX_CONFIG` | Common Nix settings. |
| `REPOSITORY_NIX_CONFIG` | Per-repository Nix settings as a JSON object. |
| `KVM_REPOSITORIES` | Comma-separated repositories that need `/dev/kvm`. The device must exist on the host. |
| `HTTPS_ROUTES` | Hostnames allowed to use HTTPS through addresses in `198.18.0.0/15`. |

## Security

The `pool` service needs `privileged` mode because it runs its own Docker Engine. Jobs run as the `gha` user without the GitHub App key or access to the Docker socket. Run only trusted code in the pool. See the security policy for deployment guidance and private reporting. [Security policy](SECURITY.md)
