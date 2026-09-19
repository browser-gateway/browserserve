//! Active capability probing: attempt each operation and clean up, so the
//! reported tier reflects what actually works (catching LSM denials that
//! capability bits miss).

use crate::linux::cgroup;
use crate::linux::tiers::{KillTier, MemCapTier, ProfileTier, Tiers};
use std::path::Path;

/// Probes the host and resolves the active isolation tiers.
#[must_use]
pub fn detect(data_dir: &Path) -> Tiers {
    let mut notes = Vec::new();
    let (memcap, kill) = probe_cgroup(&mut notes);
    let profile = probe_profile(data_dir, &mut notes);
    Tiers {
        kill,
        memcap,
        profile,
        notes,
    }
}

fn probe_cgroup(notes: &mut Vec<String>) -> (MemCapTier, KillTier) {
    // The entrypoint may have pre-delegated a uid-owned subtree; prefer it.
    let base = match std::env::var("BROWSERSERVE_CGROUP_BASE") {
        Ok(explicit) if std::path::Path::new(&explicit).is_dir() => {
            std::path::PathBuf::from(explicit)
        }
        _ => {
            let Some(dir) = cgroup::own_cgroup_dir() else {
                notes.push(String::from(
                    "cgroup: /proc/self/cgroup unreadable; using killpg + rss-poll",
                ));
                return (MemCapTier::RssPoll, KillTier::Killpg);
            };
            if !cgroup::available_controllers(&dir)
                .iter()
                .any(|c| c == "memory")
            {
                notes.push(format!(
                    "cgroup: 'memory' not delegated in {} (needs +memory in parent subtree_control); using rss-poll",
                    dir.display()
                ));
                return (MemCapTier::RssPoll, KillTier::Killpg);
            }
            dir
        }
    };
    match cgroup::Cgroup::create(&base, "browserserve-probe") {
        Ok(leaf) => {
            let writable = leaf.set_memory_max(0).is_ok();
            let can_kill = leaf.supports_kill();
            let leaf_controllers = cgroup::available_controllers(leaf.dir());
            // Verify a real process migration, not merely that memory.max is
            // writable: a delegated subtree can still refuse cgroup.procs writes
            // (EACCES) when it is not the common ancestor of the runtime and the
            // leaf, which leaves every session uncapped despite a writable
            // memory.max. This is the operation per-session capping depends on.
            let attach_ok = writable && probe_attach(&leaf);
            tokio::task::block_in_place(|| {
                let _ = std::fs::remove_dir(leaf.dir());
            });
            if attach_ok {
                notes.push(String::from(
                    "cgroup: memory.max writable and process migration works; hard cap active",
                ));
                notes.push(limits_note(&leaf_controllers));
                let kill = if can_kill {
                    KillTier::CgroupKill
                } else {
                    KillTier::Killpg
                };
                (MemCapTier::Cgroup, kill)
            } else if writable {
                notes.push(String::from(
                    "cgroup: memory.max writable but process migration denied (delegation boundary); using rss-poll",
                ));
                (MemCapTier::RssPoll, KillTier::Killpg)
            } else {
                notes.push(String::from(
                    "cgroup: leaf created but memory.max not writable; using rss-poll",
                ));
                (MemCapTier::RssPoll, KillTier::Killpg)
            }
        }
        Err(e) => {
            notes.push(format!(
                "cgroup: cannot create leaf ({e}); using killpg + rss-poll"
            ));
            (MemCapTier::RssPoll, KillTier::Killpg)
        }
    }
}

/// Confirms the delegated subtree actually permits migrating a process into a
/// leaf. Runs a throwaway child that joins the leaf and exits, and reports
/// whether it could. Conservative on any error: a host we cannot verify is treated as
/// unable to migrate, so we fall back to the RSS soft cap rather than claim a
/// hard cap we cannot apply.
/// Which per-session limits the delegated leaf can enforce, from the
/// controllers actually present in it.
fn limits_note(controllers: &[String]) -> String {
    let state = |name: &str| {
        if controllers.iter().any(|c| c == name) {
            "enforceable"
        } else {
            "not delegated"
        }
    };
    format!(
        "cgroup limits: memoryMaxMb {}, cpuPercent {}, pidsMax {}",
        state("memory"),
        state("cpu"),
        state("pids")
    )
}

fn probe_attach(leaf: &cgroup::Cgroup) -> bool {
    // Same join-then-exec path a browser launch uses, so the tier is proven
    // with the real mechanism (including the presence of the shell).
    std::process::Command::new(crate::chrome::launch::ENTER_SHELL)
        .arg("-c")
        .arg(crate::chrome::launch::ENTER_LEAF_THEN_EXEC)
        .arg(leaf.dir())
        .arg("true")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn probe_profile(data_dir: &Path, notes: &mut Vec<String>) -> ProfileTier {
    if let Err(e) = std::fs::create_dir_all(data_dir) {
        notes.push(format!("profile: data_dir uncreatable ({e}); plain-copy"));
        return ProfileTier::PlainCopy;
    }
    match reflink_copy::check_reflink_support(data_dir, data_dir) {
        Ok(reflink_copy::ReflinkSupport::Supported) => {
            notes.push(String::from(
                "profile: reflink supported on data_dir; CoW clone active",
            ));
            ProfileTier::Reflink
        }
        Ok(_) => {
            notes.push(String::from(
                "profile: reflink unsupported on data_dir; plain-copy",
            ));
            ProfileTier::PlainCopy
        }
        Err(e) => {
            notes.push(format!("profile: reflink probe failed ({e}); plain-copy"));
            ProfileTier::PlainCopy
        }
    }
}

#[cfg(test)]
mod limits_note_tests {
    use super::limits_note;

    #[test]
    fn reports_each_limit_by_its_own_controller() {
        let controllers = vec![String::from("memory"), String::from("pids")];
        assert_eq!(
            limits_note(&controllers),
            "cgroup limits: memoryMaxMb enforceable, cpuPercent not delegated, pidsMax enforceable"
        );
    }
}
