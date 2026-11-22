mod cgroup;
mod docker;
mod github;
mod pool;
mod worker_options;

use anyhow::{Context, Result, ensure};
use docker::{Docker, Worker};
use github::{Github, Job, Runner};
use pool::{Capacity, POOL_LABEL, Profile, allocate, for_labels, profile};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::{env, fs, thread};
use worker_options::WorkerOptions;

const WARMUP: u64 = 60;
const IDLE_TIMEOUT: u64 = 600;
const RUNNER_SUFFIX_LEN: usize = 18;

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

    fn should_poll(&self, repo: &str, occupied: bool, sweep: bool, interval: Duration) -> bool {
        occupied
            || sweep
            || self
                .idle_since
                .get(repo)
                .is_none_or(|t| t.elapsed() >= interval)
    }

    fn record_queue(&mut self, repo: &str, has_jobs: bool) {
        if has_jobs {
            self.idle_since.remove(repo);
        } else {
            self.idle_since.insert(repo.to_owned(), Instant::now());
        }
    }
}

fn idle_interval(repo_count: usize) -> Duration {
    // Two run-list requests per idle repo. Bound the installation's average API load.
    Duration::from_secs(60.max((repo_count as u64).saturating_mul(3)))
}

#[derive(Default)]
struct Snapshot {
    jobs: Vec<QueuedJob>,
    runners: Vec<Runner>,
}

impl Snapshot {
    fn runner_for(&self, worker: &Worker) -> Option<&Runner> {
        self.runners.iter().find(|runner| {
            runner.name == worker.name && worker.runner_id.is_none_or(|id| runner.id == id)
        })
    }
}

struct QueuedJob {
    id: u64,
    class: Profile,
    generic: bool,
    requires_pool_label: bool,
}

fn pool_jobs(jobs: Vec<Job>) -> Vec<QueuedJob> {
    jobs.into_iter()
        .filter_map(|job| {
            let class = for_labels(&job.labels)?;
            let has_label = |label| job.labels.iter().any(|l| l.eq_ignore_ascii_case(label));
            Some(QueuedJob {
                id: job.id,
                class,
                generic: !has_label(class.label),
                requires_pool_label: has_label(POOL_LABEL),
            })
        })
        .collect()
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
    let interval = idle_interval(repos.len());
    let mut snapshots = HashMap::new();
    for repo in repos {
        let watching = directory.repositories.binary_search(&repo).is_ok();
        let occupied = active.contains(repo.as_str());
        if !directory.should_poll(&repo, occupied, sweep, interval) {
            continue;
        }
        let result = (|| -> Result<Snapshot> {
            let jobs = if watching {
                pool_jobs(github.queued_jobs(&repo)?)
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
                directory.record_queue(&repo, !snapshot.jobs.is_empty());
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
        let runner = snapshot.and_then(|s| s.runner_for(w));
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
            !busy || !w.accepts_jobs || now.saturating_sub(w.created_at) < WARMUP,
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
            if is_orphaned_runner(r, prefix, &names) {
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
    for (job, repo, class) in requested {
        let mut remaining = capacity;
        let Some(memory) = allocate(&mut remaining, class) else {
            continue;
        };
        let private_cache =
            options.check_private_cache() && github.allows_private_cache(repo).unwrap_or(false);
        let extra = options.args(repo, class.cpus, private_cache)?;
        let unique = fs::read_to_string("/proc/sys/kernel/random/uuid")?;
        let name = format!("{prefix}{}-{}", class.label, &unique[..RUNNER_SUFFIX_LEN]);
        let (runner_id, config) = github.jit(repo, &name, class.label)?;
        if let Err(error) = docker.launch(&name, repo, class, memory, (runner_id, &config), &extra)
        {
            github
                .remove_runner(repo, runner_id)
                .context("clean up failed worker registration")?;
            return Err(error);
        }
        capacity = remaining;
        eprintln!(
            "Started {name} for {repo} job {job} ({} CPU, {} MiB)",
            class.cpus,
            memory / (1024 * 1024)
        );
    }
    Ok(())
}

fn is_orphaned_runner(runner: &Runner, prefix: &str, local_names: &HashSet<&str>) -> bool {
    !runner.busy
        && runner.status == "offline"
        && !local_names.contains(runner.name.as_str())
        && is_pool_runner_name(&runner.name, prefix)
}

fn is_pool_runner_name(name: &str, prefix: &str) -> bool {
    let Some(rest) = name.strip_prefix(prefix) else {
        return false;
    };
    let Some((label, suffix)) =
        rest.split_at_checked(rest.len().saturating_sub(RUNNER_SUFFIX_LEN + 1))
    else {
        return false;
    };
    profile(label).is_some_and(|class| class.label == label)
        && suffix.strip_prefix('-').is_some_and(|suffix| {
            suffix.len() == RUNNER_SUFFIX_LEN
                && suffix.bytes().enumerate().all(|(i, byte)| match i {
                    8 | 13 => byte == b'-',
                    _ => byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte),
                })
        })
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

fn match_queue(
    jobs: &[QueuedJob],
    idle: &[Idle<'_>],
    now: u64,
) -> (Vec<(u64, Profile)>, Vec<usize>) {
    let mut jobs: Vec<_> = jobs.iter().collect();
    jobs.sort_by_key(|job| (job.generic, !job.requires_pool_label, job.id));
    let mut available: Vec<_> = (0..idle.len()).collect();
    let mut missing = Vec::new();
    for job in jobs {
        let existing = available
            .iter()
            .enumerate()
            .filter_map(|(index, &i)| {
                let (w, other, runner) = idle[i];
                let marked = runner.map_or(w.runner_id.is_some(), |r| r.has_label(POOL_LABEL));
                let usable = w.accepts_jobs
                    && (job.generic || other == job.class)
                    && (!job.requires_pool_label || marked)
                    && (runner.is_some_and(|r| r.status == "online")
                        || now.saturating_sub(w.created_at) < IDLE_TIMEOUT);
                usable.then_some((marked, index))
            })
            .min();
        if let Some((_, index)) = existing {
            available.swap_remove(index);
        } else {
            missing.push((job.id, job.class));
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
            runner_id: None,
            running: true,
            accepts_jobs: true,
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

    fn runner(id: u64, name: &str, status: &str, busy: bool, marked: bool) -> Runner {
        let labels = if marked {
            vec![serde_json::json!({"name": POOL_LABEL})]
        } else {
            Vec::new()
        };
        serde_json::from_value(serde_json::json!({
            "id": id, "name": name, "status": status, "busy": busy, "labels": labels,
        }))
        .unwrap()
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
        let jobs = pool_jobs(vec![
            job(1, "self-hosted"),
            job(2, "small"),
            job(3, "small"),
        ]);
        let (missing, surplus) = match_queue(&jobs, &idle, 110);
        assert_eq!(missing, [(3, class), (1, profile("tiny").unwrap())]);
        assert!(surplus.is_empty());
    }

    #[test]
    fn expired_offline_worker_is_replaced_and_online_idle_can_cover_generic_job() {
        let worker = worker("huge");
        let class = profile("huge").unwrap();
        let online = runner(1, "test", "online", false, false);
        let jobs = pool_jobs(vec![job(1, "self-hosted")]);
        let (missing, surplus) = match_queue(&jobs, &[(&worker, class, None)], 950);
        assert_eq!(surplus, [0]);
        assert_eq!(missing, [(1, profile("tiny").unwrap())]);
        let (missing, surplus) = match_queue(&jobs, &[(&worker, class, Some(&online))], 950);
        assert!(missing.is_empty() && surplus.is_empty());
    }

    #[test]
    fn recognizes_only_current_pool_runner_names() {
        let prefix = "ghp-engine-";
        assert!(is_pool_runner_name(
            "ghp-engine-small-12345678-1234-1234",
            prefix
        ));
        assert!(is_pool_runner_name(
            "ghp-engine-large-compute-abcdef12-1234-1234",
            prefix
        ));
        assert!(!is_pool_runner_name(
            "ghp-other-small-12345678-1234-1234",
            prefix
        ));
        assert!(!is_pool_runner_name(
            "ghp-engine-small-12345678-1234-123z",
            prefix
        ));
        assert!(!is_pool_runner_name(
            "ghp-engine-custom-12345678-1234-1234",
            prefix
        ));
    }

    #[test]
    fn ignores_unsupported_queued_jobs_for_idle_polling() {
        let mut directory = Directory::default();
        let repo = "owner/repo";
        let interval = idle_interval(10);
        assert_eq!(interval, Duration::from_secs(60));
        assert_eq!(idle_interval(100), Duration::from_secs(300));
        assert!(directory.should_poll(repo, false, false, interval));

        let jobs = pool_jobs(vec![job(1, "ubuntu-latest"), job(2, "windows")]);
        directory.record_queue(repo, !jobs.is_empty());
        assert!(!directory.should_poll(repo, false, false, interval));
        assert!(directory.should_poll(repo, true, false, interval));
        assert!(directory.should_poll(repo, false, true, interval));
        assert!(directory.should_poll(repo, false, false, Duration::ZERO));
        assert!(directory.should_poll("owner/other", false, false, interval));

        let jobs = pool_jobs(vec![job(1, "ubuntu-latest"), job(2, "small")]);
        assert_eq!(jobs.iter().map(|job| job.id).collect::<Vec<_>>(), [2]);
        directory.record_queue(repo, !jobs.is_empty());
        assert!(directory.should_poll(repo, false, false, interval));
    }

    #[test]
    fn marked_jobs_require_a_marked_worker_and_legacy_jobs_keep_their_options() {
        let legacy = worker("small");
        let marked = Worker {
            runner_id: Some(2),
            ..worker("small")
        };
        let class = profile("small").unwrap();
        let mut marked_job = job(2, "small");
        marked_job.labels.push(POOL_LABEL.into());
        let jobs = pool_jobs(vec![job(1, "small"), marked_job]);

        let idle = [(&legacy, class, None)];
        assert_eq!(match_queue(&jobs, &idle, 110).0, [(2, class)]);
        let idle = [(&marked, class, None), (&legacy, class, None)];
        assert!(match_queue(&jobs, &idle, 110).0.is_empty());

        let unmarked_runner = runner(2, "test", "online", false, false);
        let idle = [(&marked, class, Some(&unmarked_runner))];
        assert_eq!(match_queue(&jobs, &idle, 110).0, [(2, class)]);
    }

    #[test]
    fn paused_or_restarting_workers_do_not_cover_queued_jobs() {
        let mut worker = worker("small");
        worker.accepts_jobs = false;
        let class = profile("small").unwrap();
        let online = runner(1, "test", "online", false, false);
        let jobs = pool_jobs(vec![job(1, "small")]);
        assert_eq!(
            match_queue(&jobs, &[(&worker, class, Some(&online))], 110),
            (vec![(1, class)], vec![0])
        );
    }

    #[test]
    fn registered_worker_uses_exact_runner_id_and_name() {
        let mut worker = worker("small");
        worker.runner_id = Some(2);
        let mut snapshot = Snapshot {
            runners: vec![runner(1, "test", "online", false, true)],
            ..Snapshot::default()
        };
        assert!(snapshot.runner_for(&worker).is_none());
        snapshot
            .runners
            .push(runner(2, "another", "online", false, true));
        assert!(snapshot.runner_for(&worker).is_none());
        snapshot
            .runners
            .push(runner(2, "test", "online", false, true));
        assert_eq!(snapshot.runner_for(&worker).unwrap().id, 2);

        worker.runner_id = None;
        assert_eq!(snapshot.runner_for(&worker).unwrap().id, 1);
    }

    #[test]
    fn cleans_up_only_marked_offline_unoccupied_registrations() {
        let prefix = "ghp-engine-";
        let name = "ghp-engine-small-12345678-1234-1234";
        let empty = HashSet::new();
        let valid = runner(1, name, "offline", false, false);
        assert!(is_orphaned_runner(&valid, prefix, &empty));
        assert!(!is_orphaned_runner(&valid, prefix, &HashSet::from([name])));
        for runner in [
            runner(2, name, "offline", true, false),
            runner(3, name, "offline", true, true),
            runner(4, name, "online", false, true),
            runner(
                5,
                "ghp-other-small-12345678-1234-1234",
                "offline",
                false,
                true,
            ),
        ] {
            assert!(!is_orphaned_runner(&runner, prefix, &empty));
        }
    }
}
