use anyhow::{Context, Result, ensure};
use std::{fs, path::Path};

use crate::pool::Capacity;

const ROOT: &str = "/sys/fs/cgroup";

pub fn budget() -> Result<Capacity> {
    read_budget(Path::new(ROOT))
}

fn read_budget(root: &Path) -> Result<Capacity> {
    let read = |name: &str| {
        fs::read_to_string(root.join(name))
            .with_context(|| format!("read cgroup v2 {name} (Compose requires cgroup: private)"))
    };
    let cpu = read("cpu.max")?;
    let (quota, period) = cpu.trim().split_once(' ').context("invalid cpu.max")?;
    let quota: u64 = quota
        .parse()
        .context("set a finite cpus limit in Compose")?;
    let period: u64 = period.parse()?;
    ensure!(
        period > 0 && quota >= period,
        "pool requires at least one CPU"
    );
    let memory: u64 = read("memory.max")?
        .trim()
        .parse()
        .context("set mem_limit in Compose")?;
    let used: u64 = read("memory.current")?.trim().parse()?;
    let stat = read("memory.stat")?;
    let inactive_file: u64 = stat
        .lines()
        .find_map(|line| line.strip_prefix("inactive_file "))
        .context("missing inactive_file in memory.stat")?
        .parse()?;
    Ok(Capacity {
        cpus: quota as f64 / period as f64,
        memory: memory.saturating_sub(used.saturating_sub(inactive_file)),
    })
}
