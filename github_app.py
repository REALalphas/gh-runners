"""GitHub App API access for the self-hosted runner controller."""

import base64
from datetime import datetime
import json
import subprocess
from threading import Lock
import time
from urllib.parse import urlencode, urlsplit
from urllib.error import HTTPError
from urllib.request import Request, urlopen


def _base64url(data):
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode("ascii")


class GitHubApp:
    def __init__(self, app_id, installation_id, key_path, openssl="openssl"):
        self.app_id = app_id
        self.installation_id = installation_id
        self.key_path = key_path
        self.openssl = openssl
        self._token = None
        self._expires_at = 0
        self._token_lock = Lock()
        self.rate_limited_until = 0

    def _jwt(self):
        now = int(time.time())
        header = _base64url(b'{"alg":"RS256","typ":"JWT"}')
        payload = _base64url(json.dumps({
            "iss": str(self.app_id), "iat": now - 60, "exp": now + 540,
        }).encode())
        body = f"{header}.{payload}"
        signature = subprocess.check_output(
            [self.openssl, "dgst", "-sha256", "-sign", str(self.key_path)],
            input=body.encode("ascii"),
        )
        return f"{body}.{_base64url(signature)}"

    def _request(self, path, token, method="GET", payload=None):
        if time.time() < self.rate_limited_until:
            raise RateLimited()
        request = Request(
            f"https://api.github.com{path}",
            data=None if payload is None else json.dumps(payload).encode(),
            method=method,
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {token}",
                "Content-Type": "application/json",
                "User-Agent": "gh-runners-controller",
                "X-GitHub-Api-Version": "2022-11-28",
            },
        )
        try:
            with urlopen(request, timeout=20) as response:
                body = response.read()
                return (json.loads(body) if body else None), response.headers
        except HTTPError as error:
            headers = error.headers
            remaining = headers.get("X-RateLimit-Remaining")
            retry = headers.get("Retry-After")
            secondary = (
                error.code == 403 and remaining != "0" and not retry
                and b"secondary rate limit" in error.read(4096).lower()
            )
            if error.code == 429 or (error.code == 403 and (remaining == "0" or retry or secondary)):
                now = time.time()
                resume = int(headers.get("X-RateLimit-Reset", "0")) + 1 if remaining == "0" else 0
                resume = max(resume, now + int(retry) if retry else 0)
                self.rate_limited_until = max(self.rate_limited_until, resume if resume > now else now + 60)
                error.close()
                raise RateLimited() from error
            raise

    def _installation_token(self):
        with self._token_lock:
            if time.time() >= self._expires_at - 60:
                data, _ = self._request(
                    f"/app/installations/{self.installation_id}/access_tokens",
                    self._jwt(), "POST", {},
                )
                self._token = data["token"]
                self._expires_at = datetime.fromisoformat(data["expires_at"]).timestamp()
            return self._token

    def permissions(self):
        jwt = self._jwt()
        app, _ = self._request("/app", jwt)
        installation, _ = self._request(f"/app/installations/{self.installation_id}", jwt)
        return app["permissions"], installation["permissions"]

    def pages(self, path, key, **query):
        path = f"{path}?{urlencode({'per_page': 100, **query})}"
        while path:
            data, headers = self._request(path, self._installation_token())
            yield from data[key]
            path = None
            for link in headers.get("Link", "").split(","):
                url, _, relation = link.strip().partition(";")
                if relation.strip() == 'rel="next"':
                    parsed = urlsplit(url.strip("<>"))
                    if parsed.netloc != "api.github.com" or parsed.scheme != "https":
                        raise ValueError("GitHub API returned an invalid pagination URL")
                    path = f"{parsed.path}?{parsed.query}"
                    break

    def repositories(self):
        return self.pages("/installation/repositories", "repositories")

    def queued_jobs(self, repository):
        runs = f"/repos/{repository}/actions/runs"
        seen = set()
        for status in ("queued", "in_progress"):
            for run in self.pages(runs, "workflow_runs", status=status):
                attempt = (run["id"], run["run_attempt"])
                if attempt in seen:
                    continue
                seen.add(attempt)
                path = f"{runs}/{attempt[0]}/attempts/{attempt[1]}/jobs"
                for job in self.pages(path, "jobs"):
                    if job["status"] == "queued" and "self-hosted" in job["labels"]:
                        yield job

    def create_runner(self, repository, name, labels):
        data, _ = self._request(
            f"/repos/{repository}/actions/runners/generate-jitconfig",
            self._installation_token(), "POST",
            {"name": name, "runner_group_id": 1, "labels": labels, "work_folder": "_work"},
        )
        return data["runner"]["id"], data["encoded_jit_config"]

    def delete_runner(self, repository, runner_id):
        self._request(
            f"/repos/{repository}/actions/runners/{runner_id}",
            self._installation_token(), "DELETE",
        )


class RateLimited(Exception):
    """GitHub has asked this installation to pause requests."""
