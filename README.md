# GitHub Runners Controller

A small controller for short-lived, self-hosted GitHub Actions runners.

The controller uses a GitHub App installation to discover repositories and queued jobs. It creates a just-in-time runner only when a matching job is waiting, starts it with a resource slot, and removes the registration after the process exits.

## Requirements

- Linux host with Docker
- Python 3.11+
- A GitHub App installed on the target account
- Repository permissions `Actions: Read and write`, `Administration: Read and write`, and `Metadata: Read-only`
- The App private key stored outside the repository

## Setup

1. Install the GitHub App on the account and copy its App ID, installation ID, and private key.
2. Copy `config.example.json` to `config.json`.
3. Set the IDs, private-key path, image name, and resource slots.
4. Build the runner image with the Dockerfile.
5. Start the controller.

```sh
python3 controller.py config.json
```

A workflow selects a slot with its labels:

```yaml
runs-on: [self-hosted, small]
```

```yaml
runs-on: [self-hosted, large-compute]
```

The controller never stores a private key or registration token in the repository. JIT runner configuration is passed directly to the short-lived process and is removed with the container.

## Security

Keep the App private. Do not commit private keys, installation tokens, runner configuration, logs containing tokens, or infrastructure addresses. Give the App only the permissions listed above. Run jobs in disposable containers with no privileged mode, no host mounts, a PID limit, fixed CPU and memory limits, and an egress policy appropriate for the host environment.

The workflow repository must decide which jobs are trusted to use self-hosted runners. Public pull requests can execute repository-controlled code, so use GitHub approval controls and review workflow changes.
