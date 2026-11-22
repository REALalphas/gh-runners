# Security policy

## Supported versions

Security fixes land on `main` and are published in the `latest` runner pool image.

## Reporting a vulnerability

Report vulnerabilities privately through GitHub. Include the affected component, image digest or Git commit, reproduction steps, and potential impact. Redact credentials, private keys, and tokens from any logs. [Report a vulnerability](https://github.com/REALalphas/gh-runners/security/advisories/new)

## Deployment assumptions

Run the privileged Docker-in-Docker pool on a dedicated host or VM. Restrict the GitHub App installation to trusted repositories and route untrusted contributions to GitHub-hosted runners. Jobs with a writable persistent cache can affect later jobs in the same repository, so enable those caches only for trusted workloads.
