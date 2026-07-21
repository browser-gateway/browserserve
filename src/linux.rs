//! Linux-only kernel-grade isolation: cgroup v2 caps, mount tiers, capability
//! probing. Every public item degrades to a safe no-op on other platforms.

#[cfg(target_os = "linux")]
pub mod cgroup;
#[cfg(target_os = "linux")]
pub mod probe;

#[cfg(not(target_os = "linux"))]
pub mod probe {
    //! Non-Linux stub: everything resolves to the portable floor.
    pub use super::tiers::{KillTier, MemCapTier, ProfileTier, Tiers};

    /// Resolves to the universal fallback tiers on non-Linux hosts.
    #[must_use]
    pub fn detect(_data_dir: &std::path::Path) -> Tiers {
        Tiers {
            kill: KillTier::Killpg,
            memcap: MemCapTier::RssPoll,
            profile: ProfileTier::PlainCopy,
            notes: vec![String::from(
                "non-Linux host: kernel isolation tiers unavailable",
            )],
        }
    }
}

pub mod tiers;
