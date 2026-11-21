"""Assign queued GitHub Actions jobs to short-lived Docker runners."""

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
from hashlib import sha256
from ipaddress import IPv4Address, IPv4Network
import json
import logging
from pathlib import Path
import re
import signal
import socket
import subprocess
import time
from urllib.error import HTTPError, URLError
from uuid import uuid4

import yaml

from github_app import GitHubApp, RateLimited


DEFAULT_LABELS = {"self-hosted", "linux", "x64"}
LOG = logging.getLogger(__name__)


def matches(labels, slot_label):
    requested = {label.lower() for label in labels}
    return requested <= DEFAULT_LABELS | {slot_label.lower()}


def uses_pool(source, slot_labels):
    """Check static runner labels, and keep workflows with computed labels eligible."""
    workflow = yaml.safe_load(source)
    if not isinstance(workflow, dict) or not isinstance(workflow.get("jobs"), dict):
        return False
    for job in workflow["jobs"].values():
        if not isinstance(job, dict):
            continue
        if "uses" in job and not str(job["uses"]).startswith("./"):
            return True
        labels = job.get("runs-on", [])
        if isinstance(labels, dict):
            if "labels" not in labels:
                return True
            labels = labels["labels"]
        if isinstance(labels, str):
            labels = [labels]
        if not isinstance(labels, list) or not all(isinstance(label, str) for label in labels):
            return True
        if any("${{" in label for label in labels):
            return True
        if labels and any(matches(labels, slot) for slot in slot_labels):
            return True
    return False


def containers(instance, engine):
    output = subprocess.check_output(
        [engine, "ps", "--all", "--filter", f"label=gh-runners.instance={instance}",
         "--format", '{{.Names}}'], text=True,
    )
    return set(output.splitlines())


def https_route_args(hostnames):
    """Use the engine host's synthetic DNS route for an explicitly named HTTPS endpoint."""
    synthetic = IPv4Network("198.18.0.0/15")
    args, allowed = [], set()
    for hostname in hostnames:
        if not re.fullmatch(r"[a-zA-Z0-9][a-zA-Z0-9.-]*\.[a-zA-Z]{2,}", hostname):
            raise ValueError(f"Invalid HTTPS route hostname: {hostname!r}")
        answers = socket.getaddrinfo(hostname, 443, socket.AF_INET, socket.SOCK_STREAM)
        address = next(
            (answer[4][0] for answer in answers if IPv4Address(answer[4][0]) in synthetic),
            None,
        )
        if address:
            args.extend(["--add-host", f"{hostname}:{address}"])
            allowed.add(address)
    if allowed:
        args.extend(["--env", f"RUNNER_HTTPS_ROUTE_IPS={' '.join(sorted(allowed))}"])
    return args


def npm_cache_args(cache_root, slot_name, repository):
    if not cache_root:
        return []
    namespace = sha256(repository.lower().encode()).hexdigest()
    directory = Path(cache_root) / slot_name / namespace
    directory.mkdir(mode=0o700, exist_ok=True)
    return ["--mount", f"type=bind,src={directory},dst=/cache"]


def launch(engine, image, instance, slot, name, jit_config, https_routes=(), kvm=False,
           cache_args=(), nix_config="", cargo_target_cache=False, nix_binary_cache=None):
    device_args = []
    if kvm:
        gid = Path("/dev/kvm").stat().st_gid
        device_args = ["--device", "/dev/kvm:/dev/kvm:rw", "--env", f"RUNNER_KVM_GID={gid}"]
    cargo_args = []
    if slot.get("cargo_build_jobs"):
        cargo_args.extend(["--env", f"CARGO_BUILD_JOBS={slot['cargo_build_jobs']}"])
    if cache_args and cargo_target_cache and slot.get("cargo_build_jobs"):
        cargo_args.extend(["--env", "CARGO_TARGET_DIR=/cache/cargo-target"])
    nix_settings = [slot.get("nix_config", ""), nix_config]
    nix_cache_args = []
    if nix_binary_cache:
        nix_cache_args = ["--mount", f"type=bind,src={nix_binary_cache},dst=/nix-cache,readonly"]
        nix_settings.append("extra-substituters = file:///nix-cache?priority=10")
    nix_config = "\n".join(filter(None, nix_settings))
    process = subprocess.Popen([
        engine, "run", "--rm", "--interactive", "--init", "--sig-proxy=false", "--name", name,
        "--label", f"gh-runners.instance={instance}",
        "--cpus", str(slot["cpus"]), "--memory", slot["memory"],
        "--memory-swap", slot["memory"], "--pids-limit", "2048",
        "--cap-drop", "ALL", "--cap-add", "CHOWN", "--cap-add", "NET_ADMIN", "--cap-add", "SETPCAP",
        "--cap-add", "SETGID", "--cap-add", "SETUID", "--cap-add", "KILL",
        "--security-opt", "no-new-privileges",
        "--dns", "1.1.1.1", "--dns", "9.9.9.9", "--read-only",
        "--tmpfs", "/run:rw,nosuid,nodev,noexec,size=16m",
        # Toolchain installers such as rustup execute binaries from /tmp.
        "--tmpfs", "/tmp:rw,exec,nosuid,nodev,mode=1777,size=512m",
        "--volume", "/runner", "--env", f"NIX_CONFIG={nix_config}",
        *https_route_args(https_routes), *device_args, *cargo_args, *cache_args, *nix_cache_args, image,
    ], stdin=subprocess.PIPE)
    try:
        with process.stdin as pipe:
            pipe.write(f"{jit_config}\n".encode("ascii"))
    except BrokenPipeError:
        pass
    return process


class Controller:
    def __init__(self, app, config):
        self.app = app
        self.config = config
        self.active = {}
        self.pending_deletions = set()
        self.repository_offset = 0
        self.repositories_cache = []
        self.repositories_updated = float("-inf")
        self.workflow_cache = {}
        self.repository_checked = {}
        self.active_attempts = {}
        self.jobs_checked = {}
        self.private_target_caches = set()

    def delete_runner(self, repository, runner_id):
        try:
            self.app.delete_runner(repository, runner_id)
        except HTTPError as error:
            if error.code != 404:
                raise

    def reap(self):
        for slot, (process, repository, runner_id, _) in list(self.active.items()):
            exit_code = process.poll()
            if exit_code is None:
                continue
            LOG.info("Slot %s exited with status %d", slot, exit_code)
            self.pending_deletions.add((repository, runner_id))
            del self.active[slot]

        for repository, runner_id in list(self.pending_deletions):
            try:
                self.delete_runner(repository, runner_id)
            except (URLError, OSError) as error:
                LOG.warning("Runner %s cleanup will be retried: %s", runner_id, error)
            else:
                self.pending_deletions.remove((repository, runner_id))

    def cycle(self):
        self.reap()
        instance = self.config["instance"]
        engine = self.config.get("engine", "docker")
        existing = containers(instance, engine)
        free = [
            slot for slot in self.config["slots"]
            if slot["name"] not in self.active
            and f"gh-runner-{instance}-{slot['name']}" not in existing
        ]
        if not free:
            return

        now = time.monotonic()
        if now >= self.repositories_updated + self.config.get("repository_refresh_seconds", 600):
            self.discover_repositories()
        active_interval = self.config.get("active_poll_seconds", 10)
        idle_interval = self.config["poll_seconds"]
        ready = [
            repo for repo in self.repositories_cache
            if now >= self.repository_checked.get(repo, float("-inf")) + idle_interval
            or (self.active_attempts.get(repo)
                and now >= self.jobs_checked.get(repo, float("-inf")) + active_interval)
        ]
        if not ready:
            return
        offset = self.repository_offset % len(ready)
        self.repository_offset += 1
        assigned = {(entry[1], entry[3]) for entry in self.active.values()}
        ordered = ready[offset:] + ready[:offset]
        ordered.sort(key=lambda repo: not self.active_attempts.get(repo))
        with ThreadPoolExecutor(max_workers=8) as pool:
            for start in range(0, len(ordered), 8):
                pending = {
                    pool.submit(self.poll_repository, repository, now): repository
                    for repository in ordered[start:start + 8]
                }
                for result in as_completed(pending):
                    repository = pending[result]
                    try:
                        jobs = result.result()
                    except HTTPError as error:
                        if error.code not in (403, 404, 409):
                            raise
                        LOG.info("Repository %s returned HTTP %d", repository, error.code)
                        self.repository_checked[repository] = now
                        self.jobs_checked[repository] = now
                        continue
                    except (URLError, OSError) as error:
                        LOG.warning("Queue check failed for %s: %s", repository, error)
                        self.jobs_checked[repository] = now
                        continue
                    for job in jobs:
                        if (repository, job["id"]) in assigned:
                            continue
                        slot = next((slot for slot in free if matches(job["labels"], slot["label"])), None)
                        if slot is None:
                            continue
                        self.start(repository, slot, job["id"])
                        free.remove(slot)
                        if not free:
                            return

    def discover_repositories(self):
        repositories = {
            repo["full_name"]: repo for repo in self.app.repositories()
            if not repo["archived"] and not repo["disabled"]
            and ("repositories" not in self.config or repo["full_name"] in self.config["repositories"])
        }
        with ThreadPoolExecutor(max_workers=8) as pool:
            private_flags = (repo.get("private", False) for repo in repositories.values())
            eligible = [
                repo for repo, match in zip(
                    repositories, pool.map(self.repository_uses_pool, repositories, private_flags),
                )
                if match
            ]
            private = [repo for repo in eligible if repositories[repo].get("private")]
            self.private_target_caches = {
                repo for repo, allowed in zip(private, pool.map(self.private_target_cache_allowed, private))
                if allowed
            } if self.config.get("cache_private_cargo_targets") else set()
        self.repositories_cache = sorted(eligible)
        self.repositories_updated = time.monotonic()
        LOG.info("Workflow discovery found %d of %d repositories for this pool", len(eligible), len(repositories))

    def private_target_cache_allowed(self, repository):
        try:
            return not self.app.allows_private_fork_workflows(repository)
        except (URLError, OSError) as error:
            LOG.info("Could not confirm private fork policy for %s: %s", repository, error)
            return False

    def repository_uses_pool(self, repository, private=False):
        try:
            slot_labels = [slot["label"] for slot in self.config["slots"]]
            files = self.app.workflow_files(repository)
            if not files and private:
                return self.app.has_active_workflows(repository)
            for sha in files:
                if sha not in self.workflow_cache:
                    self.workflow_cache[sha] = uses_pool(self.app.workflow_source(repository, sha), slot_labels)
                if self.workflow_cache[sha]:
                    return True
            return False
        except HTTPError as error:
            if error.code != 403:
                LOG.warning("Workflow discovery failed for %s: %s", repository, error)
                return True
            try:
                return self.app.has_active_workflows(repository)
            except (URLError, OSError):
                LOG.warning("Could not list workflows for %s; keeping it eligible", repository)
                return True
        except (URLError, OSError, UnicodeError, yaml.YAMLError):
            LOG.warning("Could not inspect workflows for %s; keeping it eligible", repository)
            return True

    def poll_repository(self, repository, now):
        if now >= self.repository_checked.get(repository, float("-inf")) + self.config["poll_seconds"]:
            self.active_attempts[repository] = self.app.active_runs(repository)
            self.repository_checked[repository] = now
        jobs = []
        for attempt in sorted(self.active_attempts.get(repository, ())):
            jobs.extend(job for job in self.app.run_jobs(repository, attempt)
                        if job["status"] == "queued" and job["labels"])
        self.jobs_checked[repository] = now
        return jobs

    def start(self, repository, slot, job_id):
        instance = self.config["instance"]
        name = f"gh-runner-{instance}-{slot['name']}"
        cache_args = npm_cache_args(self.config.get("cache_root"), slot["name"], repository)
        runner_id, jit_config = self.app.create_runner(
            repository, f"{slot['name']}-{uuid4().hex[:8]}",
            ["self-hosted", "Linux", "X64", slot["label"]],
        )
        try:
            process = launch(
                self.config.get("engine", "docker"), self.config["image"], instance, slot,
                name, jit_config, self.config.get("https_routes", ()),
                repository in slot.get("kvm_repositories", ()),
                cache_args,
                self.config.get("repository_nix_config", {}).get(repository, ""),
                repository in self.private_target_caches
                or repository in self.config.get("cargo_target_cache_repositories", ()),
                nix_binary_cache=self.config.get("nix_binary_cache"),
            )
        except OSError:
            self.delete_runner(repository, runner_id)
            raise
        self.active[slot["name"]] = process, repository, runner_id, job_id
        LOG.info("Started slot %s for %s", slot["name"], repository)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("config", type=Path)
    args = parser.parse_args()
    config = json.loads(args.config.read_text())
    names = [slot["name"] for slot in config["slots"]]
    if not names or len(set(names)) != len(names):
        parser.error("Configure at least one uniquely named slot")
    for name in [config["instance"], *names]:
        if not re.fullmatch(r"[a-z0-9][a-z0-9-]{0,29}", name):
            parser.error("Use lowercase letters, digits and hyphens for instance and slot names")
    if min(config["poll_seconds"], config.get("active_poll_seconds", 10),
           config.get("repository_refresh_seconds", 600)) <= 0:
        parser.error("Polling intervals must be positive")
    if config.get("engine", "docker") not in {"docker", "podman"}:
        parser.error("engine must be docker or podman")
    if cache := config.get("nix_binary_cache"):
        path = Path(cache)
        if not path.is_absolute() or not (path / "nix-cache-info").is_file():
            parser.error("nix_binary_cache must be an absolute path to a local Nix binary cache")

    logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
    app = GitHubApp(config["app_id"], config["installation_id"], config["key_path"])
    _, permissions = app.permissions()
    if permissions.get("administration") != "write" or permissions.get("actions") not in {"read", "write"}:
        parser.error("The App installation needs Administration: write and Actions: read")
    controller = Controller(app, config)

    def stop(_signum, _frame):
        raise SystemExit(0)

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    while True:
        delay = config.get("active_poll_seconds", 10)
        started = time.monotonic()
        try:
            controller.cycle()
        except RateLimited:
            delay = max(1, app.rate_limited_until - time.time())
            LOG.warning("GitHub API rate limit. Retrying in %.0f seconds", delay)
        except (HTTPError, URLError, OSError, subprocess.CalledProcessError) as error:
            LOG.warning("Polling failed: %s", error)
        else:
            delay = max(0, delay - (time.monotonic() - started))
        time.sleep(delay)


if __name__ == "__main__":
    main()
