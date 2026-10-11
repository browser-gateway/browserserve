//! The profile hand-off store: a short-lived, in-memory coat check.
//!
//! The gateway drops a profile off (`drop_off`) and gets a one-shot,
//! unguessable token. The session claims it once (`claim`) to seed the browser,
//! deposits the captured profile back under the same token (`deposit_result`),
//! and the gateway picks that up once (`pick_up`). Nothing is written to disk;
//! entries expire and are swept so an unclaimed profile cannot linger.

use crate::profile::payload::ProfilePayload;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::Instant;
use zeroize::Zeroize;

/// Token entropy in bytes (256 bits). A `getrandom` CSPRNG value, URL-safe
/// base64 encoded for the `?profileToken=` query param.
const TOKEN_BYTES: usize = 32;

/// How long a dropped-off or captured profile lives before it is swept.
pub const PROFILE_TTL: Duration = Duration::from_mins(2);

/// Backstop for a claimed profile whose session never reports back. A running
/// session keeps its entry however long it lasts; this only bounds a leak.
pub const IN_FLIGHT_MAX_AGE: Duration = Duration::from_hours(24);

/// Cap on outstanding profile entries (drop-off backpressure).
pub const MAX_PENDING: usize = 256;

/// Max accepted profile upload size. `IndexedDB` stores can be large; this caps
/// a single hand-off to bound memory. Sized from a measured store plus margin.
pub const MAX_PROFILE_BYTES: usize = 64 * 1024 * 1024;

enum Slot {
    /// Dropped off, not yet claimed by a session.
    Pending(Box<ProfilePayload>),
    /// A session claimed it and is running.
    InFlight,
    /// The session finished; the captured profile awaits pick-up.
    Ready(Box<ProfilePayload>),
}

struct Entry {
    slot: Slot,
    at: Instant,
}

/// In-memory, TTL-bounded, single-use profile hand-off store.
pub struct ProfileStore {
    entries: Mutex<HashMap<String, Entry>>,
    ttl: Duration,
    max_pending: usize,
}

impl ProfileStore {
    /// Builds a store with an entry time-to-live and a cap on outstanding
    /// (un-picked-up) entries.
    #[must_use]
    pub fn new(ttl: Duration, max_pending: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
            max_pending,
        }
    }

    /// Stores a profile and returns its one-shot token, or `None` if the store
    /// is at its outstanding-entry cap (backpressure) or the CSPRNG failed.
    #[must_use]
    pub fn drop_off(&self, payload: ProfilePayload) -> Option<String> {
        let mut map = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.sweep(&mut map);
        if map.len() >= self.max_pending {
            return None;
        }
        let token = mint_token()?;
        map.insert(
            token.clone(),
            Entry {
                slot: Slot::Pending(Box::new(payload)),
                at: Instant::now(),
            },
        );
        Some(token)
    }

    /// Claims a pending profile exactly once, moving it to in-flight. Returns
    /// `None` for an unknown, expired, or already-claimed token.
    #[must_use]
    pub fn claim(&self, token: &str) -> Option<ProfilePayload> {
        let mut map = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.sweep(&mut map);
        let entry = map.get_mut(token)?;
        if !matches!(entry.slot, Slot::Pending(_)) {
            return None;
        }
        entry.at = Instant::now();
        let Slot::Pending(payload) = std::mem::replace(&mut entry.slot, Slot::InFlight) else {
            return None;
        };
        Some(*payload)
    }

    /// Deposits the captured profile for a claimed token. No-op if the token is
    /// gone (expired) or was never in flight.
    pub fn deposit_result(&self, token: &str, payload: ProfilePayload) {
        let mut map = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(entry) = map.get_mut(token)
            && matches!(entry.slot, Slot::InFlight)
        {
            entry.slot = Slot::Ready(Box::new(payload));
            entry.at = Instant::now();
        }
    }

    /// Picks up a ready (captured) profile exactly once, removing the entry.
    #[must_use]
    pub fn pick_up(&self, token: &str) -> Option<ProfilePayload> {
        let mut map = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.sweep(&mut map);
        if !matches!(
            map.get(token),
            Some(Entry {
                slot: Slot::Ready(_),
                ..
            })
        ) {
            return None;
        }
        match map.remove(token) {
            Some(Entry {
                slot: Slot::Ready(payload),
                ..
            }) => Some(*payload),
            _ => None,
        }
    }

    /// Forgets a claimed profile whose session ended without a capture to hand back.
    pub fn finish_without_result(&self, token: &str) {
        let mut map = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if matches!(
            map.get(token),
            Some(Entry {
                slot: Slot::InFlight,
                ..
            })
        ) {
            map.remove(token);
        }
    }

    fn sweep(&self, map: &mut HashMap<String, Entry>) {
        let ttl = self.ttl;
        map.retain(|_, entry| {
            let limit = if matches!(entry.slot, Slot::InFlight) {
                IN_FLIGHT_MAX_AGE
            } else {
                ttl
            };
            entry.at.elapsed() < limit
        });
    }
}

fn mint_token() -> Option<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).ok()?;
    let token = URL_SAFE_NO_PAD.encode(bytes);
    bytes.zeroize();
    Some(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(cookie_name: &str) -> ProfilePayload {
        use crate::profile::cookie::{Cookie, SameSite};
        ProfilePayload {
            cookies: vec![Cookie {
                name: cookie_name.into(),
                value: "v".into(),
                domain: "example.com".into(),
                path: "/".into(),
                expires: -1.0,
                secure: true,
                http_only: true,
                same_site: Some(SameSite::Lax),
                priority: None,
                source_scheme: None,
                source_port: None,
                partition_key: None,
                partition_key_opaque: false,
            }],
            local_storage: vec![],
            indexeddb: vec![],
        }
    }

    fn store() -> ProfileStore {
        ProfileStore::new(Duration::from_mins(1), 100)
    }

    #[test]
    fn tokens_are_unguessable_length() {
        let s = store();
        let t = s.drop_off(payload("a")).unwrap();
        // 32 bytes url-safe-base64-no-pad = 43 chars, no '+', '/', or '='.
        assert_eq!(t.len(), 43);
        assert!(!t.contains(['+', '/', '=']));
        assert_ne!(t, s.drop_off(payload("b")).unwrap(), "tokens differ");
    }

    #[test]
    fn a_session_longer_than_the_ttl_still_returns_its_profile() {
        let s = ProfileStore::new(Duration::from_millis(50), 100);
        let token = s.drop_off(payload("long")).unwrap();
        let _ = s.claim(&token).expect("claim pending");
        std::thread::sleep(Duration::from_millis(80));
        let other = s.drop_off(payload("other")).unwrap();
        let _ = s.claim(&other);
        s.deposit_result(&token, payload("captured"));
        let picked = s
            .pick_up(&token)
            .expect("a long session's capture is still picked up");
        assert_eq!(picked.cookies[0].name, "captured");
    }

    #[test]
    fn a_session_without_a_capture_frees_its_entry() {
        let s = ProfileStore::new(Duration::from_mins(1), 1);
        let token = s.drop_off(payload("ro")).unwrap();
        let _ = s.claim(&token).expect("claim pending");
        assert!(
            s.drop_off(payload("next")).is_none(),
            "the running session holds the only slot"
        );
        s.finish_without_result(&token);
        assert!(
            s.drop_off(payload("next")).is_some(),
            "the slot is free once the session ends"
        );
    }

    #[test]
    fn full_round_trip_drop_claim_deposit_pickup() {
        let s = store();
        let token = s.drop_off(payload("sess")).unwrap();

        let claimed = s.claim(&token).expect("claim pending");
        assert_eq!(claimed.cookies[0].name, "sess");

        s.deposit_result(&token, payload("captured"));
        let picked = s.pick_up(&token).expect("pick up ready");
        assert_eq!(picked.cookies[0].name, "captured");
    }

    #[test]
    fn claim_is_single_use() {
        let s = store();
        let token = s.drop_off(payload("a")).unwrap();
        assert!(s.claim(&token).is_some());
        assert!(s.claim(&token).is_none(), "second claim must fail");
    }

    #[test]
    fn pickup_before_ready_is_none_and_unknown_token_is_none() {
        let s = store();
        let token = s.drop_off(payload("a")).unwrap();
        assert!(s.pick_up(&token).is_none(), "not ready yet");
        s.claim(&token).unwrap();
        assert!(s.pick_up(&token).is_none(), "in-flight, not ready");
        assert!(s.claim("nope").is_none());
        assert!(s.pick_up("nope").is_none());
    }

    #[test]
    fn pickup_is_single_use() {
        let s = store();
        let token = s.drop_off(payload("a")).unwrap();
        s.claim(&token).unwrap();
        s.deposit_result(&token, payload("c"));
        assert!(s.pick_up(&token).is_some());
        assert!(s.pick_up(&token).is_none(), "second pick-up must fail");
    }

    #[test]
    fn max_pending_applies_backpressure() {
        let s = ProfileStore::new(Duration::from_mins(1), 2);
        assert!(s.drop_off(payload("a")).is_some());
        assert!(s.drop_off(payload("b")).is_some());
        assert!(s.drop_off(payload("c")).is_none(), "over the cap");
    }

    #[tokio::test(start_paused = true)]
    async fn entries_expire_after_ttl() {
        let s = ProfileStore::new(Duration::from_millis(50), 100);
        let token = s.drop_off(payload("a")).unwrap();
        tokio::time::advance(Duration::from_millis(80)).await;
        assert!(s.claim(&token).is_none(), "expired entry is swept");
    }
}
