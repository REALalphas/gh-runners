"""Assign queued GitHub Actions jobs to short-lived Docker runners."""

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
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

from github_app import GitHubApp


DEFAULT_LABELS = {"self-hosted", "linux", "x64"}
LOG = logging.getLogger(__name__)


def matches(labels, slot_label):
    requested = {label.lower() for label in labels}
    return requested <= DEFAULT_LABELS | {slot_label.lower()}


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


def launch(engine, image, instance, slot, name, jit_config, https_routes=(), kvm=False):
    device_args = []
    if kvm:
        gid = Path("/dev/kvm").stat().st_gid
        device_args = ["--device", "/dev/kvm:/dev/kvm:rw", "--env", f"RUNNER_KVM_GID={gid}"]
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
        "--tmpfs", "/tmp:rw,nosuid,nodev,mode=1777,size=512m",
        "--volume", "/runner", *https_route_args(https_routes), *device_args, image,
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
        self.repository_offset = 0

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
            self.delete_runner(repository, runner_id)
            del self.active[slot]

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

        repositories = sorted(
            repo["full_name"] for repo in self.app.repositories()
            if not repo["archived"] and not repo["disabled"]
            and ("repositories" not in self.config or repo["full_name"] in self.config["repositories"])
        )
        if not repositories:
            return
        offset = self.repository_offset % len(repositories)
        self.repository_offset += 1
        assigned = {(entry[1], entry[3]) for entry in self.active.values()}
        ordered = repositories[offset:] + repositories[:offset]
        with ThreadPoolExecutor(max_workers=8) as pool:
            for start in range(0, len(ordered), 8):
                pending = {
                    pool.submit(lambda repo: list(self.app.queued_jobs(repo)), repository): repository
                    for repository in ordered[start:start + 8]
                }
                for result in as_completed(pending):
                    repository = pending[result]
                    try:
                        jobs = result.result()
                    except HTTPError as error:
                        if error.code not in (404, 409):
                            raise
                        LOG.info("Repository %s returned HTTP %d", repository, error.code)
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

    def start(self, repository, slot, job_id):
        instance = self.config["instance"]
        name = f"gh-runner-{instance}-{slot['name']}"
        runner_id, jit_config = self.app.create_runner(
            repository, f"{slot['name']}-{uuid4().hex[:8]}",
            ["self-hosted", "Linux", "X64", slot["label"]],
        )
        try:
            process = launch(
                self.config.get("engine", "docker"), self.config["image"], instance, slot,
                name, jit_config, self.config.get("https_routes", ()),
                repository in slot.get("kvm_repositories", ()),
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
    if config["poll_seconds"] <= 0:
        parser.error("poll_seconds must be positive")
    if config.get("engine", "docker") not in {"docker", "podman"}:
        parser.error("engine must be docker or podman")

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
        try:
            controller.cycle()
        except (HTTPError, URLError, OSError, subprocess.CalledProcessError) as error:
            LOG.warning("Polling failed: %s", error)
        time.sleep(config["poll_seconds"])


if __name__ == "__main__":
    main()
