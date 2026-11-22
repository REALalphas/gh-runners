use crate::pool::Profile;
use anyhow::{Context, Result, anyhow, ensure};
use reqwest::blocking::{Client, Response};
use reqwest::{Method, StatusCode};
use serde::Deserialize;
use std::collections::HashMap;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

const OWNER_LABEL: &str = "io.t3code.runner-pool.owner";
const PROFILE_LABEL: &str = "io.t3code.runner-pool.profile";
const REPOSITORY_LABEL: &str = "io.t3code.runner-pool.repository";

pub struct Docker {
    image: String,
    prefix: String,
    host: String,
    client: Client,
}

#[derive(Debug)]
pub struct Worker {
    pub name: String,
    pub profile: String,
    pub repository: String,
    pub running: bool,
    pub created_at: u64,
    pub cpu_usage: f64,
    pub memory_usage: u64,
    pub memory_limit: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Container {
    id: String,
    names: Vec<String>,
    labels: HashMap<String, String>,
    state: String,
    created: u64,
}

impl Container {
    fn name(&self) -> Result<&str> {
        self.names
            .first()
            .and_then(|name| name.strip_prefix('/'))
            .filter(|name| !name.is_empty())
            .context("Docker returned a container without a name")
    }

    fn running(&self) -> bool {
        // A paused or restarting worker still occupies a pool slot.
        matches!(self.state.as_str(), "running" | "paused" | "restarting")
    }
}

impl Docker {
    pub fn new(image: &str, prefix: &str) -> Result<Self> {
        ensure!(!prefix.is_empty(), "Docker worker prefix must be nonempty");
        let host = std::env::var("DOCKER_HOST").context("read private Docker DOCKER_HOST")?;
        let socket = host
            .strip_prefix("unix://")
            .filter(|path| path.starts_with('/'))
            .context("DOCKER_HOST must specify an absolute Unix socket")?;
        let client = Client::builder()
            .unix_socket(socket)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .context("build Docker Engine client")?;
        Ok(Self {
            image: image.to_owned(),
            prefix: prefix.to_owned(),
            host,
            client,
        })
    }

    pub fn workers(&self) -> Result<Vec<Worker>> {
        let containers = self.containers()?;
        let mut workers = Vec::with_capacity(containers.len());
        // Docker may wait a sampling interval per live container. Sample in
        // parallel with a bounded number of requests to the private daemon.
        for batch in containers.chunks(16) {
            std::thread::scope(|scope| -> Result<()> {
                let samples: Vec<_> = batch
                    .iter()
                    .map(|container| scope.spawn(|| self.worker(container)))
                    .collect();
                for sample in samples {
                    if let Some(worker) = sample
                        .join()
                        .map_err(|_| anyhow!("worker sampling panicked"))??
                    {
                        workers.push(worker);
                    }
                }
                Ok(())
            })?;
        }
        Ok(workers)
    }

    fn worker(&self, container: &Container) -> Result<Option<Worker>> {
        let name = container.name()?.to_owned();
        let profile = container
            .labels
            .get(PROFILE_LABEL)
            .filter(|profile| !profile.is_empty())
            .with_context(|| format!("worker {name} has no profile label"))?
            .clone();
        let mut running = container.running();
        let usage = if running {
            let class = crate::pool::profile(&profile).context("unknown worker profile")?;
            match self.usage(&container.id, class.cpus) {
                Ok(usage) => usage,
                Err(error) => {
                    let current = self
                        .containers()?
                        .into_iter()
                        .find(|c| c.id == container.id);
                    match current {
                        None => return Ok(None),
                        Some(c) if !c.running() => {
                            running = false;
                            Usage::default()
                        }
                        Some(_) => return Err(error.context(format!("sample worker {name}"))),
                    }
                }
            }
        } else {
            Usage::default()
        };
        Ok(Some(Worker {
            name,
            profile,
            repository: container
                .labels
                .get(REPOSITORY_LABEL)
                .cloned()
                .unwrap_or_default(),
            running,
            created_at: container.created,
            cpu_usage: usage.cores,
            memory_usage: usage.memory,
            memory_limit: usage.memory_limit,
        }))
    }

    pub fn launch(
        &self,
        name: &str,
        repo: &str,
        class: Profile,
        memory: u64,
        jit: &str,
        extra: &[String],
    ) -> Result<()> {
        ensure!(
            !repo.is_empty(),
            "Docker worker repository must be nonempty"
        );
        ensure!(
            !class.label.is_empty(),
            "Docker worker profile must be nonempty"
        );
        ensure!(!jit.is_empty(), "Docker worker JIT config must be nonempty");
        ensure!(
            class.cpus > 0 && memory > 0,
            "Docker worker limits must be positive"
        );

        let created = self
            .command()
            .arg("create")
            .args(extra)
            .args([
                "--name",
                name,
                "--label",
                &format!("{OWNER_LABEL}={}", self.prefix),
                "--label",
                &format!("{PROFILE_LABEL}={}", class.label),
                "--label",
                &format!("{REPOSITORY_LABEL}={repo}"),
                "--cpus",
                &class.cpus.to_string(),
                "--memory",
                &memory.to_string(),
                "--memory-swap",
                &memory.to_string(),
                "--pids-limit",
                "2048",
                "--init",
                "--rm",
                "--read-only",
                "--cap-drop",
                "ALL",
                "--cap-add",
                "CHOWN",
                "--cap-add",
                "KILL",
                "--cap-add",
                "NET_ADMIN",
                "--cap-add",
                "SETGID",
                "--cap-add",
                "SETPCAP",
                "--cap-add",
                "SETUID",
                "--security-opt",
                "no-new-privileges:true",
                "--dns",
                "1.1.1.1",
                "--dns",
                "9.9.9.9",
                // Parallel A and AAAA queries can stall DNS on the runner network.
                "--dns-option",
                "single-request",
                "--tmpfs",
                "/run:rw,nosuid,nodev,noexec,mode=0755,size=16m",
                "--tmpfs",
                "/tmp:rw,nosuid,nodev,exec,mode=1777,size=512m",
                "--volume",
                "/runner",
                "--env",
                "RUNNER_JIT_CONFIG",
                "--",
                &self.image,
            ])
            .env("RUNNER_JIT_CONFIG", jit)
            .stdin(Stdio::null())
            .output()
            .context("execute docker create")?;
        check_launch_output("create", &created)?;
        let id = validated_id(
            std::str::from_utf8(&created.stdout)
                .context("Docker create returned a non-UTF-8 container ID")?
                .trim(),
        )?;

        let started = self
            .command()
            .args(["start", id])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .output()
            .context("execute docker start")
            .and_then(|output| check_launch_output("start", &output));
        if let Err(error) = started {
            if let Err(cleanup) = self.remove_id(id) {
                return Err(error.context(format!("worker cleanup failed: {cleanup:#}")));
            }
            return Err(error);
        }
        Ok(())
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        for container in self.containers()? {
            if container.name()? == name {
                return self.remove_id(&container.id);
            }
        }
        Ok(())
    }

    fn containers(&self) -> Result<Vec<Container>> {
        let filters = serde_json::json!({"label": [format!("{OWNER_LABEL}={}", self.prefix)]});
        let response = self
            .client
            .get("http://docker/containers/json")
            .query(&[("all", "true"), ("filters", &filters.to_string())])
            .send()
            .context("request Docker container list")?;
        ensure!(
            response.status() == StatusCode::OK,
            "Docker container list returned HTTP {}",
            response.status()
        );
        let containers: Vec<Container> = response.json().context("decode Docker container list")?;
        Ok(containers
            .into_iter()
            .filter(|container| container.labels.get(OWNER_LABEL) == Some(&self.prefix))
            .collect())
    }

    fn usage(&self, id: &str, limit: u32) -> Result<Usage> {
        let id = validated_id(id)?;
        let response =
            self.request(Method::GET, &format!("/containers/{id}/stats?stream=false"))?;
        ensure!(
            response.status() == StatusCode::OK,
            "Docker stats returned HTTP {}",
            response.status()
        );
        let stats: Stats = response.json().context("decode Docker stats")?;
        stats.usage(limit)
    }

    fn remove_id(&self, id: &str) -> Result<()> {
        let id = validated_id(id)?;
        let response = self.request(
            Method::DELETE,
            &format!("/containers/{id}?v=true&force=true"),
        )?;
        ensure!(
            matches!(
                response.status(),
                StatusCode::NO_CONTENT | StatusCode::NOT_FOUND
            ),
            "Docker container removal returned HTTP {}",
            response.status()
        );
        Ok(())
    }

    fn command(&self) -> Command {
        let mut command = Command::new("docker");
        command
            .args(["--host", &self.host])
            .env_remove("DOCKER_CONTEXT");
        command
    }

    fn request(&self, method: Method, path: &str) -> Result<Response> {
        self.client
            .request(method.clone(), format!("http://docker{path}"))
            .send()
            .with_context(|| format!("Docker Engine {method} request"))
    }
}

fn check_launch_output(operation: &str, output: &Output) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }

    // CLI and daemon diagnostics can contain container configuration, including
    // its environment. Report recognized failures without echoing their text.
    let stderr = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
    let reason = if stderr.contains("cannot connect to the docker daemon") {
        "cannot connect to the Docker daemon"
    } else if stderr.contains("permission denied") {
        "permission denied"
    } else if stderr.contains("container name") && stderr.contains("already in use") {
        "container name already in use"
    } else if stderr.contains("no such image") || stderr.contains("pull access denied") {
        "worker image unavailable"
    } else if stderr.contains("unknown flag") || stderr.contains("invalid argument") {
        "invalid Docker options"
    } else if stderr.contains("executable file not found") {
        "worker executable missing"
    } else if stderr.contains("oci runtime") {
        "OCI runtime failed to start the worker"
    } else {
        "Docker CLI or daemon rejected the operation"
    };
    Err(anyhow!(
        "docker {operation} exited with {}: {reason}",
        output.status
    ))
}

fn validated_id(id: &str) -> Result<&str> {
    ensure!(
        id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "Docker returned an invalid container ID"
    );
    Ok(id)
}

#[derive(Deserialize)]
struct Stats {
    cpu_stats: CpuStats,
    #[serde(default)]
    precpu_stats: CpuStats,
    memory_stats: MemoryStats,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CpuStats {
    cpu_usage: CpuUsage,
    system_cpu_usage: u64,
    online_cpus: u32,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CpuUsage {
    total_usage: u64,
    percpu_usage: Vec<u64>,
}

#[derive(Deserialize)]
struct MemoryStats {
    usage: u64,
    limit: u64,
    #[serde(default)]
    stats: HashMap<String, u64>,
}

#[derive(Debug, Default, PartialEq)]
struct Usage {
    cores: f64,
    memory: u64,
    memory_limit: u64,
}

impl Stats {
    fn usage(&self, limit: u32) -> Result<Usage> {
        let current = &self.cpu_stats;
        let previous = &self.precpu_stats;
        let cpus = if current.online_cpus > 0 {
            current.online_cpus as usize
        } else {
            current.cpu_usage.percpu_usage.len()
        };
        // Docker can return memory and a current CPU sample without a baseline.
        // Reserve its CPU limit until Docker has both samples.
        let cores = if cpus == 0 || previous.system_cpu_usage == 0 {
            f64::from(limit)
        } else {
            let system_delta = current
                .system_cpu_usage
                .checked_sub(previous.system_cpu_usage)
                .filter(|delta| *delta > 0)
                .context("Docker stats missing system CPU delta")?;
            let cpu_delta = current
                .cpu_usage
                .total_usage
                .checked_sub(previous.cpu_usage.total_usage)
                .context("Docker stats CPU counter reset")?;
            cpu_delta as f64 / system_delta as f64 * cpus as f64
        };
        ensure!(
            self.memory_stats.usage > 0 && self.memory_stats.limit > 0,
            "Docker stats missing memory sample"
        );
        // Match Docker's working-set calculation for cgroup v1 and v2.
        let cache = self
            .memory_stats
            .stats
            .get("total_inactive_file")
            .or_else(|| self.memory_stats.stats.get("inactive_file"))
            .copied()
            .filter(|cache| *cache < self.memory_stats.usage)
            .unwrap_or(0);
        Ok(Usage {
            cores,
            memory: self.memory_stats.usage - cache,
            memory_limit: self.memory_stats.limit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats() -> serde_json::Value {
        serde_json::json!({
            "cpu_stats": {
                "cpu_usage": {"total_usage": 700, "percpu_usage": [1, 2, 3, 4]},
                "system_cpu_usage": 2000,
                "online_cpus": 4
            },
            "precpu_stats": {
                "cpu_usage": {"total_usage": 200},
                "system_cpu_usage": 1000
            },
            "memory_stats": {"usage": 1234567, "limit": 5000000, "stats": {"inactive_file": 123}}
        })
    }

    #[test]
    fn parses_live_cores_and_working_set_bytes() -> Result<()> {
        let mut value = stats();
        let sample: Stats = serde_json::from_value(value.clone())?;
        assert_eq!(
            sample.usage(1)?,
            Usage {
                cores: 2.0,
                memory: 1_234_444,
                memory_limit: 5_000_000
            }
        );

        value["cpu_stats"]
            .as_object_mut()
            .unwrap()
            .remove("online_cpus");
        value["cpu_stats"]["cpu_usage"]["total_usage"] = 200.into();
        value["memory_stats"]["stats"] = serde_json::json!({"total_inactive_file": 567});
        let sample: Stats = serde_json::from_value(value)?;
        assert_eq!(
            sample.usage(1)?,
            Usage {
                cores: 0.0,
                memory: 1_234_000,
                memory_limit: 5_000_000
            }
        );
        Ok(())
    }

    #[test]
    fn reserves_cpu_without_a_baseline() -> Result<()> {
        let mut value = stats();
        value["precpu_stats"]["system_cpu_usage"] = 0.into();
        let expected = Usage {
            cores: 1.0,
            memory: 1_234_444,
            memory_limit: 5_000_000,
        };
        assert_eq!(
            serde_json::from_value::<Stats>(value.clone())?.usage(1)?,
            expected
        );
        value["precpu_stats"] = serde_json::json!({"cpu_usage": {"total_usage": 0}});
        assert_eq!(
            serde_json::from_value::<Stats>(value.clone())?.usage(1)?,
            expected
        );
        value.as_object_mut().unwrap().remove("precpu_stats");
        assert_eq!(serde_json::from_value::<Stats>(value)?.usage(1)?, expected);
        assert!(serde_json::from_str::<Stats>(r#"{"memory_stats":{}}"#).is_err());
        Ok(())
    }

    // Run only with DOCKER_HOST pointing to a dedicated test daemon.
    // GH_RUNNER_TEST_IMAGE needs only a default command that stays running.
    #[test]
    #[ignore = "requires a dedicated DOCKER_HOST and GH_RUNNER_TEST_IMAGE"]
    fn docker_runtime_scratch_smoke() -> Result<()> {
        let image = std::env::var("GH_RUNNER_TEST_IMAGE")
            .context("set GH_RUNNER_TEST_IMAGE to an image with a long-running default command")?;
        ensure!(!image.is_empty(), "GH_RUNNER_TEST_IMAGE must be nonempty");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let name = format!("runner-scratch-{}-{nonce}", std::process::id());
        let docker = Docker::new(&image, &name)?;
        let class = crate::pool::profile("tiny").context("missing tiny profile")?;
        let repo = "scratch/smoke";
        let memory = 128 * 1024 * 1024;
        ensure!(
            docker.workers()?.is_empty(),
            "scratch pool must start empty"
        );

        let extra_label = "io.t3code.runner-pool.smoke";
        let sample = (|| -> Result<()> {
            docker.launch(
                &name,
                repo,
                class,
                memory,
                "scratch",
                &[format!("--label={extra_label}=true")],
            )?;
            ensure!(
                docker.containers()?.iter().any(|container| {
                    container
                        .labels
                        .get(extra_label)
                        .is_some_and(|value| value == "true")
                }),
                "scratch worker is missing the extra Docker label"
            );
            let workers = docker.workers()?;
            ensure!(workers.len() == 1, "expected one scratch worker");
            let worker = &workers[0];
            ensure!(
                worker.name == name && worker.profile == class.label && worker.repository == repo,
                "scratch worker identity did not round-trip"
            );
            ensure!(
                worker.running
                    && worker.created_at > 0
                    && worker.cpu_usage.is_finite()
                    && worker.cpu_usage >= 0.0
                    && worker.memory_usage > 0
                    && worker.memory_limit == memory,
                "unexpected scratch worker stats: {worker:?}"
            );
            Ok(())
        })();
        if let Err(cleanup) = docker.remove(&name) {
            return match sample {
                Ok(()) => Err(cleanup.context("remove scratch worker")),
                Err(error) => {
                    Err(error.context(format!("scratch worker cleanup failed: {cleanup:#}")))
                }
            };
        }
        sample?;
        ensure!(docker.workers()?.is_empty(), "scratch worker still present");
        docker.remove(&name)?;
        ensure!(docker.workers()?.is_empty(), "scratch pool must end empty");
        Ok(())
    }

    #[test]
    fn parses_container_summary() -> Result<()> {
        let container: Container = serde_json::from_value(serde_json::json!({
            "Id": "abc",
            "Names": ["/pool-tiny-1"],
            "Labels": {OWNER_LABEL: "pool", PROFILE_LABEL: "tiny"},
            "State": "exited",
            "Created": 1700000000
        }))?;
        assert_eq!(container.name()?, "pool-tiny-1");
        assert_eq!(container.created, 1_700_000_000);
        assert_eq!(container.labels[OWNER_LABEL], "pool");
        assert!(!container.running());
        Ok(())
    }
}
