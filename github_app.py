"""GitHub App API access for the self-hosted runner controller."""

import base64
from datetime import datetime
import json
import subprocess
import time
from urllib.parse import urlencode, urlsplit
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

    @staticmethod
    def _request(path, token, method="GET", payload=None):
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
        with urlopen(request, timeout=20) as response:
            body = response.read()
            return (json.loads(body) if body else None), response.headers

    def _installation_token(self):
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
        for status in ("queued", "in_progress"):
            for run in self.pages(runs, "workflow_runs", status=status):
                path = f"{runs}/{run['id']}/attempts/{run['run_attempt']}/jobs"
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
