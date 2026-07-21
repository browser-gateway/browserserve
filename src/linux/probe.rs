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
            let kill = if leaf.supports_kill() {
                KillTier::CgroupKill
            } else {
                KillTier::Killpg
            };
            tokio::task::block_in_place(|| {
                let _ = std::fs::remove_dir(leaf.dir());
            });
            if writable {
                notes.push(String::from("cgroup: memory.max writable; hard cap active"));
                (MemCapTier::Cgroup, kill)
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
