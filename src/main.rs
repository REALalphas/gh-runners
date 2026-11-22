mod cgroup;
mod docker;
mod github;
mod pool;
mod worker_options;

use anyhow::{Context, Result, ensure};
use docker::{Docker, Worker};
use github::{Github, Job, Runner};
use pool::{Capacity, Profile, allocate, for_labels, profile};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, thread};
use worker_options::WorkerOptions;

const WARMUP: u64 = 60;
const IDLE_TIMEOUT: u64 = 600;

fn required(key: &str) -> Result<String> {
    env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .with_context(|| format!("set {key} in compose.yaml"))
}

fn main() -> Result<()> {
    let mut github = Github::new(
        &required("GITHUB_APP_ID")?,
        &required("GITHUB_INSTALLATION_ID")?,
        &fs::read("/run/secrets/github_app_key")
            .context("read GitHub App PEM from Compose secret")?,
    )?;
    let poll: u64 = required("POLL_SECONDS")?
        .parse()
        .context("invalid POLL_SECONDS")?;
    ensure!(poll > 0, "POLL_SECONDS must be positive");
    let options = WorkerOptions::from_env()?;
    cgroup::budget()?;
    if env::args().nth(1).as_deref() == Some("--check") {
        return Ok(());
    }
    // The daemon ID survives controller restarts in the Compose engine volume.
    let engine =
        fs::read_to_string("/var/lib/docker/engine-id").context("read Docker Engine ID")?;
    let id = engine.trim();
    ensure!(
        !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "invalid engine ID"
    );
    let prefix = format!("ghp-{}-", &id[..id.len().min(12)]);
    let docker = Docker::new("gh-runner-worker:local", &prefix)?;
    let mut directory = Directory::default();
    eprintln!("Polling GitHub every {poll}s, runner prefix {prefix}");
    let mut failures = 0;
    loop {
        match reconcile(&mut github, &docker, &prefix, &mut directory, &options) {
            Ok(()) => failures = 0,
            Err(error) => {
                failures = (failures + 1u32).min(5);
                eprintln!("Pool reconciliation failed: {error:#}");
            }
        }
        thread::sleep(Duration::from_secs(
            poll.saturating_mul(1 << failures).min(poll.max(300)),
        ));
    }
}

#[derive(Default)]
struct Directory {
    repositories: Vec<String>,
    refreshed: Option<Instant>,
    idle_since: HashMap<String, Instant>,
}

impl Directory {
    fn refresh(&mut self, github: &mut Github) -> Result<bool> {
        if self
            .refreshed
            .is_some_and(|t| t.elapsed() < Duration::from_secs(600))
        {
            return Ok(false);
        }
        let selected = env::var("GITHUB_REPOSITORIES").unwrap_or_default();
        let mut repos: Vec<String> = selected
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_lowercase)
            .collect();
        if repos.is_empty() {
            repos = github
                .repositories()?
                .into_iter()
                .map(|s| s.to_ascii_lowercase())
                .collect();
        }
        repos.sort();
        repos.dedup();
        self.repositories = repos;
        self.refreshed = Some(Instant::now());
        Ok(true)
    }
}

#[derive(Default)]
struct Snapshot {
    jobs: Vec<Job>,
    runners: Vec<Runner>,
}

fn reconcile(
    github: &mut Github,
    docker: &Docker,
    prefix: &str,
    directory: &mut Directory,
    options: &WorkerOptions,
) -> Result<()> {
    let sweep = directory.refresh(github)?;
    let workers = docker.workers()?;
    let active: HashSet<_> = workers.iter().map(|w| w.repository.as_str()).collect();
    let mut repos = directory.repositories.clone();
    repos.extend(active.iter().map(|s| s.to_string()));
    repos.sort();
    repos.dedup();
    // Two run-list requests per idle repo. Spread the API budget across the installation.
    let idle_interval = Duration::from_secs(60.max(repos.len() as u64 * 3));
    let mut snapshots = HashMap::new();
    for repo in repos {
        let watching = directory.repositories.binary_search(&repo).is_ok();
        let occupied = active.contains(repo.as_str());
        if !occupied
            && !sweep
            && directory
                .idle_since
                .get(&repo)
                .is_some_and(|t| t.elapsed() < idle_interval)
        {
            continue;
        }
        let result = (|| -> Result<Snapshot> {
            let jobs = if watching {
                github.queued_jobs(&repo)?
            } else {
                Vec::new()
            };
            let runners = if occupied || sweep || !jobs.is_empty() {
                github.runners(&repo)?
            } else {
                Vec::new()
            };
            Ok(Snapshot { jobs, runners })
        })();
        match result {
            Ok(snapshot) => {
                if snapshot.jobs.is_empty() {
                    directory.idle_since.insert(repo.clone(), Instant::now());
                } else {
                    directory.idle_since.remove(&repo);
                }
                snapshots.insert(repo, snapshot);
            }
            Err(e) => eprintln!("Queue {repo}: {e:#}"),
        }
    }

    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let mut capacity = cgroup::budget()?;
    let mut idle_by_repo: HashMap<&str, Vec<Idle<'_>>> = HashMap::new();
    for w in &workers {
        let snapshot = snapshots.get(&w.repository);
        let runner = snapshot.and_then(|s| s.runners.iter().find(|r| r.name == w.name));
        if !w.running {
            if let Some(r) = runner {
                github.remove_runner(&w.repository, r.id)?;
            }
            docker.remove(&w.name)?;
            continue;
        }
        let class = profile(&w.profile).context("worker has unknown profile")?;
        let busy = runner.is_some_and(|r| r.busy);
        account(
            &mut capacity,
            w,
            class,
            !busy || now.saturating_sub(w.created_at) < WARMUP,
        );
        if !busy && snapshot.is_some() {
            idle_by_repo
                .entry(&w.repository)
                .or_default()
                .push((w, class, runner));
        }
    }

    let names: HashSet<_> = workers.iter().map(|w| w.name.as_str()).collect();
    let mut requested = Vec::new();
    for (repo, snapshot) in &snapshots {
        for r in &snapshot.runners {
            if r.name.starts_with(prefix)
                && !names.contains(r.name.as_str())
                && !r.busy
                && r.status == "offline"
            {
                github.remove_runner(repo, r.id)?;
            }
        }
        let idle = idle_by_repo
            .get(repo.as_str())
            .map(Vec::as_slice)
            .unwrap_or_default();
        let (missing, surplus) = match_queue(&snapshot.jobs, idle, now);
        for i in surplus {
            let (w, _, runner) = idle[i];
            if now.saturating_sub(w.created_at) >= IDLE_TIMEOUT {
                if let Some(r) = runner {
                    github.remove_runner(repo, r.id)?;
                }
                docker.remove(&w.name)?;
            }
        }
        requested.extend(missing.into_iter().map(|(job, class)| (job, repo, class)));
    }
    requested.sort_by_key(|(id, _, _)| *id);
    let mut launches = Vec::new();
    let prepare = (|| -> Result<()> {
        for (job, repo, class) in requested {
            let mut remaining = capacity;
            let Some(memory) = allocate(&mut remaining, class) else {
                continue;
            };
            let private_cache =
                options.check_private_cache() && github.allows_private_cache(repo).unwrap_or(false);
            let extra = options.args(repo, class.cpus, private_cache)?;
            let unique = fs::read_to_string("/proc/sys/kernel/random/uuid")?;
            let name = format!("{prefix}{}-{}", class.label, &unique[..18]);
            let (runner_id, config) = github.jit(repo, &name, class.label)?;
            launches.push(LaunchRequest {
                job,
                repo: repo.to_owned(),
                class,
                memory,
                runner_id,
                name,
                config,
                extra,
            });
            capacity = remaining;
        }
        Ok(())
    })();
    if let Err(error) = prepare {
        if let Err(cleanup) = cleanup_registered_runners(github, &launches) {
            return Err(error.context(format!("worker registration cleanup failed: {cleanup:#}")));
        }
        return Err(error);
    }

    // Runner registrations are prepared above, then all containers are created
    // and started concurrently so a busy queue does not launch one at a time.
    let results = std::thread::scope(|scope| {
        launches
            .iter()
            .map(|launch| {
                scope.spawn(|| {
                    docker.launch(
                        &launch.name,
                        &launch.repo,
                        launch.class,
                        launch.memory,
                        &launch.config,
                        &launch.extra,
                    )
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .map_err(|_| anyhow::anyhow!("worker launch panicked"))
                    .and_then(|result| result)
            })
            .collect::<Vec<_>>()
    });

    let mut first_error = None;
    for (launch, result) in launches.iter().zip(results) {
        if let Err(error) = result {
            let error = error.context(format!("launch worker {}", launch.name));
            if let Err(cleanup) = github.remove_runner(&launch.repo, launch.runner_id) {
                if first_error.is_none() {
                    first_error = Some(
                        error.context(format!("clean up failed worker registration: {cleanup:#}")),
                    );
                }
            } else if first_error.is_none() {
                first_error = Some(error);
            }
            continue;
        }
        eprintln!(
            "Started {} for {} job {} ({} CPU, {} MiB)",
            launch.name,
            launch.repo,
            launch.job,
            launch.class.cpus,
            launch.memory / (1024 * 1024)
        );
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    Ok(())
}

fn cleanup_registered_runners(github: &mut Github, launches: &[LaunchRequest]) -> Result<()> {
    let mut first_error = None;
    for launch in launches {
        if let Err(error) = github.remove_runner(&launch.repo, launch.runner_id)
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    if let Some(error) = first_error {
        return Err(error);
    }
    Ok(())
}

fn account(capacity: &mut Capacity, w: &Worker, class: Profile, reserve: bool) {
    // The outer cgroup already includes each worker's live memory.
    let cpu = if reserve {
        w.cpu_usage.max(f64::from(class.cpus))
    } else {
        w.cpu_usage
    };
    capacity.cpus = (capacity.cpus - cpu).max(0.0);
    if reserve {
        capacity.memory = capacity
            .memory
            .saturating_sub(w.memory_limit.saturating_sub(w.memory_usage));
    }
}

type Idle<'a> = (&'a Worker, Profile, Option<&'a Runner>);

struct LaunchRequest {
    job: u64,
    repo: String,
    class: Profile,
    memory: u64,
    runner_id: u64,
    name: String,
    config: String,
    extra: Vec<String>,
}

fn match_queue(jobs: &[Job], idle: &[Idle<'_>], now: u64) -> (Vec<(u64, Profile)>, Vec<usize>) {
    let mut jobs: Vec<_> = jobs
        .iter()
        .filter_map(|job| {
            let class = for_labels(&job.labels)?;
            let generic = !job
                .labels
                .iter()
                .any(|l| l.eq_ignore_ascii_case(class.label));
            Some((generic, job.id, class))
        })
        .collect();
    jobs.sort_by_key(|&(generic, id, _)| (generic, id));
    let mut available: Vec<_> = (0..idle.len()).collect();
    let mut missing = Vec::new();
    for (generic, id, class) in jobs {
        let existing = available.iter().position(|&i| {
            let (w, other, runner) = idle[i];
            (generic || other == class)
                && (runner.is_some_and(|r| r.status == "online")
                    || now.saturating_sub(w.created_at) < IDLE_TIMEOUT)
        });
        if let Some(index) = existing {
            available.swap_remove(index);
        } else {
            missing.push((id, class));
        }
    }
    (missing, available)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pool::GIB;

    fn worker(label: &str) -> Worker {
        Worker {
            name: "test".into(),
            repository: "owner/repo".into(),
            running: true,
            created_at: 100,
            profile: label.into(),
            cpu_usage: 10.0,
            memory_usage: 8 * GIB,
            memory_limit: 12 * GIB,
        }
    }

    fn job(id: u64, label: &str) -> Job {
        Job {
            id,
            labels: vec![label.into()],
        }
    }

    #[test]
    fn running_huge_leaves_six_cores_and_ten_gib_for_new_jobs() {
        let mut capacity = Capacity {
            cpus: 16.0,
            memory: 10 * GIB,
        };
        let huge = worker("huge");
        account(&mut capacity, &huge, profile("huge").unwrap(), false);
        let tiny = profile("tiny-compute").unwrap();
        for _ in 0..6 {
            assert_eq!(allocate(&mut capacity, tiny), Some(GIB));
        }
        assert!(allocate(&mut capacity, tiny).is_none());
        assert_eq!(capacity.memory, 4 * GIB);
    }

    #[test]
    fn warmup_reserves_only_the_effective_memory_limit() {
        let mut capacity = Capacity {
            cpus: 24.0,
            memory: 10 * GIB,
        };
        account(
            &mut capacity,
            &worker("huge"),
            profile("huge").unwrap(),
            true,
        );
        assert_eq!(
            capacity,
            Capacity {
                cpus: 8.0,
                memory: 6 * GIB
            }
        );
    }

    #[test]
    fn starting_worker_covers_one_specific_job_before_generic_jobs() {
        let worker = worker("small");
        let class = profile("small").unwrap();
        let idle = [(&worker, class, None)];
        let jobs = [job(1, "self-hosted"), job(2, "small"), job(3, "small")];
        let (missing, surplus) = match_queue(&jobs, &idle, 110);
        assert_eq!(missing, [(3, class), (1, profile("tiny").unwrap())]);
        assert!(surplus.is_empty());
    }

    #[test]
    fn expired_offline_worker_is_replaced_and_online_idle_can_cover_generic_job() {
        let worker = worker("huge");
        let class = profile("huge").unwrap();
        let online = Runner {
            name: "test".into(),
            status: "online".into(),
            id: 1,
            busy: false,
        };
        let jobs = [job(1, "self-hosted")];
        let (missing, surplus) = match_queue(&jobs, &[(&worker, class, None)], 950);
        assert_eq!(surplus, [0]);
        assert_eq!(missing, [(1, profile("tiny").unwrap())]);
        let (missing, surplus) = match_queue(&jobs, &[(&worker, class, Some(&online))], 950);
        assert!(missing.is_empty() && surplus.is_empty());
    }
}
