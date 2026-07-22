//! Host-measured session capacity: how many concurrent sessions this host can
//! actually hold, derived from real limits (cgroup memory/pids, cores) and the
//! measured footprint of a browser launched on this host — never a constant.

/// Hard ceilings the host imposes, read at startup.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostLimits {
    /// Memory ceiling in bytes: cgroup v2 `memory.max` when finite, else total
    /// system memory. `None` when nothing is readable.
    pub mem_ceiling_bytes: Option<u64>,
    /// Finite cgroup v2 `pids.max` (counts threads). `None` when unlimited or
    /// not in a cgroup.
    pub pids_max: Option<u64>,
    /// Logical CPUs visible to the process.
    pub cpus: u32,
}

/// Measured cost of one warmed browser on this host.
#[derive(Debug, Clone, Copy)]
pub struct SessionFootprint {
    /// Resident bytes of the browser's whole process tree.
    pub bytes: u64,
    /// Threads across the browser's whole process tree (each consumes a PID
    /// slot under the cgroup pids controller).
    pub threads: u64,
}

/// The computed ceiling and which constraint produced it.
#[derive(Debug, Clone, Copy)]
pub struct Capacity {
    /// Safe concurrent-session ceiling for this host.
    pub max_sessions: u32,
    /// The binding constraint: `memory`, `pids`, or `cpu`.
    pub bound_by: &'static str,
}

const USABLE_NUM: u64 = 4;
const USABLE_DEN: u64 = 5;
const SESSIONS_PER_CPU: u32 = 2;
const AUTO_CEILING: u32 = 256;

/// Derives the session ceiling from host limits and a measured footprint.
/// Explicit operator configuration is applied by the caller and always wins.
#[must_use]
pub fn compute(limits: HostLimits, footprint: Option<SessionFootprint>) -> Capacity {
    let cpu_bound = u64::from(limits.cpus.max(1)) * u64::from(SESSIONS_PER_CPU);
    let mut best = (cpu_bound, "cpu");
    let mut consider = |value: Option<u64>, label: &'static str| {
        if let Some(v) = value
            && v < best.0
        {
            best = (v, label);
        }
    };

    if let Some(fp) = footprint {
        let mem = limits
            .mem_ceiling_bytes
            .filter(|_| fp.bytes > 0)
            .map(|ceiling| ceiling / USABLE_DEN * USABLE_NUM / fp.bytes);
        consider(mem, "memory");
        let pids = limits
            .pids_max
            .filter(|_| fp.threads > 0)
            .map(|max| max / USABLE_DEN * USABLE_NUM / fp.threads);
        consider(pids, "pids");
    }

    let (value, bound_by) = best;
    Capacity {
        max_sessions: u32::try_from(value.clamp(1, u64::from(AUTO_CEILING))).unwrap_or(1),
        bound_by,
    }
}

/// Reads the host's ceilings. On Linux this prefers cgroup v2 files (treating
/// the `max` sentinel as unlimited) and falls back to `/proc/meminfo`;
/// elsewhere it uses total system memory.
#[must_use]
pub fn probe_host() -> HostLimits {
    let cpus =
        u32::try_from(std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get))
            .unwrap_or(u32::MAX);
    HostLimits {
        mem_ceiling_bytes: imp::mem_ceiling_bytes(),
        pids_max: imp::pids_max(),
        cpus,
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::path::Path;

    pub(super) fn mem_ceiling_bytes() -> Option<u64> {
        parse_limit_file(Path::new("/sys/fs/cgroup/memory.max")).or_else(meminfo_total)
    }

    pub(super) fn pids_max() -> Option<u64> {
        parse_limit_file(Path::new("/sys/fs/cgroup/pids.max"))
    }

    fn parse_limit_file(path: &Path) -> Option<u64> {
        let raw = std::fs::read_to_string(path).ok()?;
        let trimmed = raw.trim();
        if trimmed == "max" {
            return None;
        }
        trimmed.parse().ok()
    }

    fn meminfo_total() -> Option<u64> {
        let raw = std::fs::read_to_string("/proc/meminfo").ok()?;
        let line = raw.lines().find(|l| l.starts_with("MemTotal:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb.saturating_mul(1024))
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub(super) fn mem_ceiling_bytes() -> Option<u64> {
        let mut system = sysinfo::System::new();
        system.refresh_memory();
        let total = system.total_memory();
        (total > 0).then_some(total)
    }

    pub(super) fn pids_max() -> Option<u64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FP: SessionFootprint = SessionFootprint {
        bytes: 150 * 1024 * 1024,
        threads: 70,
    };

    fn limits(mem_gb: Option<u64>, pids: Option<u64>, cpus: u32) -> HostLimits {
        HostLimits {
            mem_ceiling_bytes: mem_gb.map(|g| g * 1024 * 1024 * 1024),
            pids_max: pids,
            cpus,
        }
    }

    #[test]
    fn memory_binds_on_a_small_box() {
        let cap = compute(limits(Some(1), None, 48), Some(FP));
        assert_eq!(cap.bound_by, "memory");
        assert!(cap.max_sessions >= 1 && cap.max_sessions < 8);
    }

    #[test]
    fn pids_bind_on_a_thread_capped_container() {
        let cap = compute(limits(Some(64), Some(1000), 48), Some(FP));
        assert_eq!(cap.bound_by, "pids");
        assert_eq!(cap.max_sessions, 1000 / 5 * 4 / 70);
    }

    #[test]
    fn cpu_binds_when_memory_and_pids_are_plentiful() {
        let cap = compute(limits(Some(512), None, 4), Some(FP));
        assert_eq!(cap.bound_by, "cpu");
        assert_eq!(cap.max_sessions, 8);
    }

    #[test]
    fn no_measurement_still_yields_cpu_bound() {
        let cap = compute(limits(Some(8), Some(1000), 8), None);
        assert_eq!(cap.bound_by, "cpu");
        assert_eq!(cap.max_sessions, 16);
    }

    #[test]
    fn nothing_readable_still_floors_on_cpu() {
        let cap = compute(
            HostLimits {
                mem_ceiling_bytes: None,
                pids_max: None,
                cpus: 0,
            },
            None,
        );
        assert_eq!(cap.max_sessions, 2);
        assert_eq!(cap.bound_by, "cpu");
    }

    #[test]
    fn floor_is_one_session() {
        let tiny = SessionFootprint {
            bytes: 10 * 1024 * 1024 * 1024,
            threads: 100_000,
        };
        let cap = compute(limits(Some(1), Some(100), 1), Some(tiny));
        assert_eq!(cap.max_sessions, 1);
    }

    #[test]
    fn auto_ceiling_caps_huge_hosts() {
        let cap = compute(limits(Some(4096), None, 999), Some(FP));
        assert!(cap.max_sessions <= 256);
    }
}
