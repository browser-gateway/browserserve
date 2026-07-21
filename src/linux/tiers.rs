//! The resolved isolation tiers, surfaced by `doctor` and `/ready`.

use serde::Serialize;

/// How a session's process tree is killed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum KillTier {
    /// `cgroup.kill`: atomic, forkbomb-safe whole-subtree SIGKILL (kernel ≥5.14).
    CgroupKill,
    /// `killpg` SIGTERM→grace→SIGKILL on the process group (universal fallback).
    Killpg,
}

/// How a session's memory is capped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum MemCapTier {
    /// cgroup `memory.max`: a hard, kernel-enforced ceiling.
    Cgroup,
    /// RSS polling soft-cap: breach is detected and the session killed after the fact.
    RssPoll,
    /// No memory cap in effect.
    None,
}

/// How a session's profile directory is provisioned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileTier {
    /// Overlay of a sealed read-only template over a tmpfs upper.
    Overlay,
    /// Copy-on-write reflink clone of the sealed template (btrfs/xfs).
    Reflink,
    /// Plain copy of the sealed template into an existing tmpfs.
    TmpfsCopy,
    /// Plain recursive copy into the data directory (universal default).
    PlainCopy,
}

/// The resolved isolation tiers for this host.
#[derive(Debug, Clone, Serialize)]
pub struct Tiers {
    /// Active kill mechanism.
    pub kill: KillTier,
    /// Active memory-cap mechanism.
    pub memcap: MemCapTier,
    /// Active profile-dir mechanism.
    pub profile: ProfileTier,
    /// Human-readable notes: why a tier resolved as it did, denial messages, etc.
    pub notes: Vec<String>,
}

impl Tiers {
    /// A one-line summary for logs and `doctor`.
    #[must_use]
    pub fn summary(&self) -> String {
        format!(
            "kill={:?} memcap={:?} profile={:?}",
            self.kill, self.memcap, self.profile
        )
    }
}
