//! Process-tree RSS sampling: the portable memory soft-cap that works
//! wherever cgroups do not.

/// Sums the resident set size (bytes) of a process and all its descendants.
///
/// Returns `None` when the process is gone or `/proc` is unavailable (e.g. on
/// macOS, where this is a compile-time no-op returning `None`).
#[must_use]
pub fn tree_rss_bytes(root_pid: i32) -> Option<u64> {
    imp::tree_rss_bytes(root_pid)
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;

    pub(super) fn tree_rss_bytes(root_pid: i32) -> Option<u64> {
        let all = procfs::process::all_processes().ok()?;
        let mut rss_by_pid: HashMap<i32, u64> = HashMap::new();
        let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
        let page_size = procfs::page_size();

        for proc in all.flatten() {
            let Ok(stat) = proc.stat() else { continue };
            #[allow(clippy::cast_possible_wrap)]
            let pid = stat.pid;
            let rss_bytes = stat.rss.saturating_mul(page_size);
            rss_by_pid.insert(pid, rss_bytes);
            children.entry(stat.ppid).or_default().push(pid);
        }

        if !rss_by_pid.contains_key(&root_pid) {
            return None;
        }

        let mut total = 0u64;
        let mut stack = vec![root_pid];
        let mut seen = std::collections::HashSet::new();
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue;
            }
            total = total.saturating_add(rss_by_pid.get(&pid).copied().unwrap_or(0));
            if let Some(kids) = children.get(&pid) {
                stack.extend(kids);
            }
        }
        Some(total)
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub(super) fn tree_rss_bytes(_root_pid: i32) -> Option<u64> {
        None
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn own_process_tree_has_nonzero_rss() {
        let pid = std::process::id() as i32;
        let rss = tree_rss_bytes(pid).expect("own process must be visible");
        assert!(rss > 0, "self RSS should be positive");
    }

    #[test]
    fn missing_pid_returns_none() {
        assert!(tree_rss_bytes(0x7FFF_FFFF).is_none());
    }
}
