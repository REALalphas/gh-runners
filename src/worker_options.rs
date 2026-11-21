use std::{
    collections::{BTreeSet, HashMap},
    env::{self, VarError},
    fs,
    io::ErrorKind,
    net::{IpAddr, ToSocketAddrs},
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail, ensure};

pub struct WorkerOptions {
    cache_root: Option<PathBuf>,
    cargo_target_repositories: BTreeSet<String>,
    cache_private_cargo_targets: bool,
    kvm_repositories: BTreeSet<String>,
    nix_binary_cache: Option<PathBuf>,
    nix_config: String,
    repository_nix_config: HashMap<String, String>,
    https_routes: BTreeSet<String>,
}

impl WorkerOptions {
    pub fn from_env() -> Result<Self> {
        let cache_root = absolute_path("CACHE_ROOT")?;
        let nix_binary_cache = absolute_path("NIX_BINARY_CACHE")?;
        if let Some(path) = &nix_binary_cache {
            ensure!(
                path.join("nix-cache-info").is_file(),
                "NIX_BINARY_CACHE must contain a nix-cache-info file"
            );
        }

        let cache_private_cargo_targets = match setting("CACHE_PRIVATE_CARGO_TARGETS")?
            .to_ascii_lowercase()
            .as_str()
        {
            "" | "0" | "false" | "no" | "off" => false,
            "1" | "true" | "yes" | "on" => true,
            _ => bail!("CACHE_PRIVATE_CARGO_TARGETS must be a boolean"),
        };

        let mut repository_nix_config = HashMap::new();
        let config = setting("REPOSITORY_NIX_CONFIG")?;
        if !config.is_empty() {
            let entries: HashMap<String, String> = serde_json::from_str(&config).context(
                "REPOSITORY_NIX_CONFIG must be a JSON object of repository config strings",
            )?;
            for (repo, config) in entries {
                let repo = repository_name(&repo)?;
                ensure!(
                    repository_nix_config.insert(repo.clone(), config).is_none(),
                    "Duplicate repository in REPOSITORY_NIX_CONFIG: {repo}"
                );
            }
        }

        let routes = setting("HTTPS_ROUTES")?;
        let mut https_routes = BTreeSet::new();
        for hostname in list(&routes) {
            ensure!(
                valid_hostname(hostname),
                "Invalid HTTPS route hostname: {hostname:?}"
            );
            https_routes.insert(hostname.to_ascii_lowercase());
        }

        Ok(Self {
            cache_root,
            cargo_target_repositories: repository_list("CARGO_TARGET_CACHE_REPOSITORIES")?,
            cache_private_cargo_targets,
            kvm_repositories: repository_list("KVM_REPOSITORIES")?,
            nix_binary_cache,
            nix_config: setting("NIX_CONFIG")?,
            repository_nix_config,
            https_routes,
        })
    }

    pub fn check_private_cache(&self) -> bool {
        self.cache_root.is_some() && self.cache_private_cargo_targets
    }

    /// Pass private_cache only after GitHub confirms the repository is private
    /// and fork workflows are disabled.
    pub fn args(&self, repo: &str, cpus: u32, private_cache: bool) -> Result<Vec<String>> {
        ensure!(cpus > 0, "Worker CPU count must be positive");
        let repo = repository_name(repo)?;
        let mut args = vec!["--env".into(), format!("CARGO_BUILD_JOBS={cpus}")];

        if let Some(root) = &self.cache_root {
            let directory = package_cache(root, &repo)?;
            args.extend([
                "--mount".into(),
                format!("type=bind,src={},dst=/cache", directory.display()),
            ]);
            if self.cargo_target_repositories.contains(&repo)
                || (self.check_private_cache() && private_cache)
            {
                args.extend([
                    "--env".into(),
                    "CARGO_TARGET_DIR=/cache/cargo-target".into(),
                ]);
            }
        }

        if self.kvm_repositories.contains(&repo) {
            let gid = fs::metadata("/dev/kvm")
                .context("Read /dev/kvm for worker")?
                .gid();
            args.extend([
                "--device".into(),
                "/dev/kvm:/dev/kvm:rw".into(),
                "--env".into(),
                format!("RUNNER_KVM_GID={gid}"),
            ]);
        }

        let cores = format!("cores = {cpus}");
        let mut nix_settings = vec![cores.as_str(), self.nix_config.as_str()];
        if let Some(config) = self.repository_nix_config.get(&repo) {
            nix_settings.push(config);
        }
        if let Some(path) = &self.nix_binary_cache {
            args.extend([
                "--mount".into(),
                format!("type=bind,src={},dst=/nix-cache,readonly", path.display()),
            ]);
            nix_settings.push("extra-substituters = file:///nix-cache?priority=10&trusted=false");
        }
        nix_settings.retain(|setting| !setting.is_empty());
        args.extend([
            "--env".into(),
            format!("NIX_CONFIG={}", nix_settings.join("\n")),
        ]);

        let mut allowed = BTreeSet::new();
        for hostname in &self.https_routes {
            let addresses = (hostname.as_str(), 443)
                .to_socket_addrs()
                .with_context(|| format!("Resolve HTTPS route hostname {hostname}"))?;
            if let Some(address) = addresses
                .filter_map(|address| match address.ip() {
                    IpAddr::V4(ip) if matches!(ip.octets(), [198, 18 | 19, _, _]) => Some(ip),
                    _ => None,
                })
                .next()
            {
                args.extend(["--add-host".into(), format!("{hostname}:{address}")]);
                allowed.insert(address.to_string());
            }
        }
        if !allowed.is_empty() {
            let ips = allowed.into_iter().collect::<Vec<_>>().join(" ");
            args.extend(["--env".into(), format!("RUNNER_HTTPS_ROUTE_IPS={ips}")]);
        }
        Ok(args)
    }
}

fn setting(name: &str) -> Result<String> {
    match env::var(name) {
        Ok(value) => Ok(value),
        Err(VarError::NotPresent) => Ok(String::new()),
        Err(error) => Err(error).with_context(|| format!("Read {name}")),
    }
}

fn absolute_path(name: &str) -> Result<Option<PathBuf>> {
    let value = setting(name)?;
    if value.is_empty() {
        return Ok(None);
    }
    let path = PathBuf::from(&value);
    ensure!(
        path.is_absolute()
            && !value.contains([',', '"', '\r', '\n'])
            && !value.split('/').any(|part| matches!(part, "." | "..")),
        "{name} must be an absolute path usable in a Docker bind mount"
    );
    Ok(Some(path))
}

fn list(value: &str) -> impl Iterator<Item = &str> {
    value
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|part| !part.is_empty())
}

fn repository_list(name: &str) -> Result<BTreeSet<String>> {
    list(&setting(name)?)
        .map(repository_name)
        .collect::<Result<_>>()
        .with_context(|| format!("Read {name}"))
}

fn repository_name(repo: &str) -> Result<String> {
    let valid = repo.split_once('/').is_some_and(|(owner, name)| {
        !owner.is_empty()
            && owner
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            && !name.is_empty()
            && !matches!(name, "." | "..")
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
    });
    ensure!(valid, "Invalid repository name: {repo:?}");
    Ok(repo.to_ascii_lowercase())
}

fn valid_hostname(hostname: &str) -> bool {
    hostname.rsplit_once('.').is_some_and(|(prefix, suffix)| {
        hostname.len() <= 253
            && suffix.len() >= 2
            && suffix.bytes().all(|c| c.is_ascii_alphabetic())
            && prefix.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            })
            && suffix.len() <= 63
    })
}

fn package_cache(root: &Path, repo: &str) -> Result<PathBuf> {
    // BusyBox sha256sum is available in the Docker-in-Docker image.
    // Hash the canonical repository name using the original cache namespace.
    let digest = Command::new("sh")
        .args(["-c", "printf '%s' \"$1\" | sha256sum", "sha256sum", repo])
        .output()
        .context("Hash repository cache namespace")?;
    ensure!(digest.status.success(), "sha256sum failed");
    let hash = std::str::from_utf8(&digest.stdout)?
        .split_ascii_whitespace()
        .next()
        .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .context("Invalid repository cache hash")?;
    let directory = root.join(hash);
    if let Err(error) = fs::DirBuilder::new().mode(0o700).create(&directory)
        && error.kind() != ErrorKind::AlreadyExists
    {
        return Err(error).with_context(|| format!("Create worker cache {}", directory.display()));
    }
    ensure!(
        directory.symlink_metadata()?.is_dir(),
        "Worker cache must be a directory: {}",
        directory.display()
    );
    Ok(directory)
}
