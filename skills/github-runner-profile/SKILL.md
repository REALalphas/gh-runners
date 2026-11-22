---
name: github-runner-profile
description: Selects and applies a self-hosted GitHub Actions runner profile for each eligible job based on its commands, tools, and peak CPU and memory usage. Use when moving a workflow to this container pool or reviewing its runs-on setting. Pool configuration is outside this skill's scope.
---

# GitHub Actions runner profiles

Review the workflow and select the smallest suitable pool profile for each eligible job. Work with the existing YAML and preserve the job structure, matrices, steps, permissions, and labels that identify a required platform or special runner.

## Available profiles

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

Profiles ending in `-compute` are for CPU-heavy jobs that fit within the lower memory limit. Use a regular profile when memory is the tighter constraint. Browsers, large dependencies, Nix, end-to-end tests, and integration tests often need a regular profile, but use the job's actual concurrency and service footprint when available.

The pool recognizes one profile label and ignores `self-hosted`, `Linux`, and `X64` when selecting it. A job that has only `self-hosted` is assigned `tiny`. A profile can therefore be the complete `runs-on` value, or it can be included with the compatibility labels.

## Process

1. Find every workflow file and every job with `runs-on`. Consider job-level `env`, matrices, services, containers, artifacts, caches, and the commands in each step.
2. Keep an existing self-hosted profile when it already fits and the user did not ask to recalculate resources.
3. For jobs with several concurrent processes, estimate peak CPU demand from the number of workers rather than average usage. Consider `make -j`, `cargo build`, npm, test workers, browsers, and services.
4. Estimate peak memory from the heaviest concurrent processes, caches, Docker images, and service containers. Include headroom for the runner and filesystem cache.
5. Select the smallest profile whose CPU and memory both cover those estimates. Compare profiles by CPU first, then memory. For equal CPU, choose the regular profile when the job needs more memory and the `-compute` profile when the lower memory limit is sufficient.
6. Use `huge` or `huge-compute` only when the job's measured or clearly inferred demand requires 16 CPUs or more than 16 GiB of memory. If information is missing, choose the smallest safe profile and state the assumption.
7. For a matrix, use one profile sufficient for its heaviest combination. Do not create different labels inside the matrix unless the workflow explicitly requires it.
8. If `runs-on` is an array, preserve `self-hosted`, `Linux`, and `X64`. Replace conflicting profile labels so the final value contains at most one recognized profile. Treat any other label as unsupported by this pool, leave that job unchanged, and report that it needs a separate runner.
9. If `runs-on` is a string, set it to the profile for this pool. When migrating from a GitHub-hosted runner, use `[self-hosted, Linux, X64, PROFILE]` when explicit compatibility labels are needed.
10. Do not change `ubuntu-latest` or another hosted runner unless the user asks to migrate to the self-hosted pool.
11. Do not change `runs-on` for jobs that need Windows, macOS, ARM, or special hardware when no matching pool profile exists. Report that the job needs a separate runner.
12. After editing, parse the workflow YAML with a repository-available validator, make sure no job has conflicting profile labels, check that all profile labels are valid, and inspect the diff for secrets.

## Quick guidelines

- linting, formatting, short scripts, and one test process use `tiny-compute` or `tiny`.
- one regular build, a Node/Python/Rust project, or Docker without heavy parallelism uses `small` or `small-compute`.
- a build with several workers, a medium test suite, or Playwright with one browser uses `medium` or `medium-compute`.
- several browsers, a Nix build, a large monorepo, or a Docker build with parallel layers uses `large` or `large-compute`.
- large integration suites, several service containers, or a job expected to use more than 16 GiB of memory uses `huge` or `huge-compute`.

Use these as starting points alongside the commands and project settings. Use explicit limits such as `-j`, `--workers`, `CARGO_BUILD_JOBS`, `NIX_CONFIG`, and matrix values when they are present. Classify Docker and Nix jobs from their configured concurrency and service footprint.

## Result

Edit the workflow through the repository's normal entry point. In the final response, list the old and new `runs-on` value for every changed job, give a short reason for each choice, and include the commands used to validate the YAML. If information is missing, choose the smallest safe profile and state the assumption.
