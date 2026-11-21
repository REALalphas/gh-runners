pub const GIB: u64 = 1024 * 1024 * 1024;
const DOCKER_MIN_MEMORY: u64 = 6 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Capacity {
    pub cpus: f64,
    pub memory: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Profile {
    pub label: &'static str,
    pub cpus: u32,
    pub memory: u64,
}

pub fn profile(label: &str) -> Option<Profile> {
    let (label, cpus, memory_gib) = [
        ("tiny", 1, 2),
        ("tiny-compute", 1, 1),
        ("small", 2, 4),
        ("small-compute", 2, 2),
        ("medium", 4, 8),
        ("medium-compute", 4, 4),
        ("large", 8, 16),
        ("large-compute", 8, 8),
        ("huge", 16, 32),
        ("huge-compute", 16, 16),
    ]
    .into_iter()
    .find(|(known_label, _, _)| known_label.eq_ignore_ascii_case(label))?;

    Some(Profile {
        label,
        cpus,
        memory: memory_gib * GIB,
    })
}

pub fn for_labels(labels: &[String]) -> Option<Profile> {
    let mut self_hosted = false;
    let mut requested = None;

    for label in labels {
        if label.eq_ignore_ascii_case("self-hosted") {
            self_hosted = true;
        } else if !label.eq_ignore_ascii_case("Linux") && !label.eq_ignore_ascii_case("X64") {
            if requested.is_some() {
                return None;
            }
            requested = Some(profile(label)?);
        }
    }

    requested.or_else(|| if self_hosted { profile("tiny") } else { None })
}

pub fn allocate(capacity: &mut Capacity, profile: Profile) -> Option<u64> {
    let cpus = f64::from(profile.cpus);
    let memory = profile.memory.min(capacity.memory);
    if !(capacity.cpus >= cpus && memory >= DOCKER_MIN_MEMORY) {
        return None;
    }

    capacity.cpus -= cpus;
    capacity.memory -= memory;
    Some(memory)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn select(labels: &[&str]) -> Option<Profile> {
        for_labels(
            &labels
                .iter()
                .map(|label| (*label).to_owned())
                .collect::<Vec<_>>(),
        )
    }

    #[test]
    fn selects_all_profiles_with_or_without_compatible_defaults() {
        for (label, cpus, memory_gib) in [
            ("tiny", 1, 2),
            ("tiny-compute", 1, 1),
            ("small", 2, 4),
            ("small-compute", 2, 2),
            ("medium", 4, 8),
            ("medium-compute", 4, 4),
            ("large", 8, 16),
            ("large-compute", 8, 8),
            ("huge", 16, 32),
            ("huge-compute", 16, 16),
        ] {
            let expected = Some(Profile {
                label,
                cpus,
                memory: memory_gib * GIB,
            });
            assert_eq!(select(&[label]), expected);
            assert_eq!(
                select(&["x64", &label.to_uppercase(), "SELF-HOSTED", "linux"]),
                expected
            );
            assert_eq!(select(&["LiNuX", label]), expected);
        }
    }

    #[test]
    fn generic_self_hosted_jobs_use_tiny() {
        for labels in [
            &["self-hosted"][..],
            &["Linux", "self-hosted"],
            &["X64", "Self-Hosted", "Linux"],
        ] {
            assert_eq!(select(labels), profile("tiny"));
        }
    }

    #[test]
    fn rejects_incompatible_or_ambiguous_labels() {
        for labels in [
            &[][..],
            &["Linux", "X64"],
            &["ubuntu-latest"],
            &["self-hosted", "ARM64"],
            &["self-hosted", "Windows", "tiny"],
            &["tiny", "custom"],
            &["self-hosted", "tiny", "huge-compute"],
        ] {
            assert_eq!(select(labels), None);
        }
    }

    #[test]
    fn fits_six_tiny_compute_jobs_on_six_cores_with_ten_gib() {
        let tiny_compute = profile("tiny-compute").unwrap();
        let mut capacity = Capacity {
            cpus: 6.0,
            memory: 10 * GIB,
        };
        for _ in 0..6 {
            assert_eq!(allocate(&mut capacity, tiny_compute), Some(GIB));
        }
        assert_eq!(allocate(&mut capacity, tiny_compute), None);
        assert_eq!(
            capacity,
            Capacity {
                cpus: 0.0,
                memory: 4 * GIB
            }
        );
    }

    #[test]
    fn clips_huge_to_the_pool_memory() {
        let mut capacity = Capacity {
            cpus: 16.0,
            memory: 24 * GIB,
        };
        assert_eq!(
            allocate(&mut capacity, profile("huge").unwrap()),
            Some(24 * GIB)
        );
        assert_eq!(
            capacity,
            Capacity {
                cpus: 0.0,
                memory: 0
            }
        );
    }

    #[test]
    fn clips_to_remaining_headroom_in_bytes() {
        let mut capacity = Capacity {
            cpus: 17.0,
            memory: 32 * GIB + 1,
        };
        assert_eq!(
            allocate(&mut capacity, profile("tiny").unwrap()),
            Some(2 * GIB)
        );
        assert_eq!(
            allocate(&mut capacity, profile("huge").unwrap()),
            Some(30 * GIB + 1)
        );
        assert_eq!(
            capacity,
            Capacity {
                cpus: 0.0,
                memory: 0
            }
        );
    }

    #[test]
    fn refuses_insufficient_resources_without_debiting_capacity() {
        let small = profile("small").unwrap();
        for mut capacity in [
            Capacity {
                cpus: 1.99,
                memory: 4 * GIB,
            },
            Capacity {
                cpus: 2.0,
                memory: DOCKER_MIN_MEMORY - 1,
            },
        ] {
            let before = capacity;
            assert_eq!(allocate(&mut capacity, small), None);
            assert_eq!(capacity, before);
        }

        let mut capacity = Capacity {
            cpus: 2.0,
            memory: DOCKER_MIN_MEMORY,
        };
        assert_eq!(allocate(&mut capacity, small), Some(DOCKER_MIN_MEMORY));
    }
}
