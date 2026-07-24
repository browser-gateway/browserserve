//! The profile as it travels the gateway <-> browserserve channel.

use crate::profile::cdp::OriginState;
use crate::profile::cookie::Cookie;
use crate::profile::manifest::ManifestEntry;
use serde::{Deserialize, Serialize};
use std::fmt;

/// A full profile payload: the portable core (cookies + localStorage) plus the
/// browserserve-native layer (`IndexedDB` / service-worker files, as a manifest).
///
/// Its `Debug` is redacted: cookie and storage VALUES are secrets and must never
/// reach a log. Only counts are printed.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ProfilePayload {
    /// Portable cookies (full CDP field set; sanitized on inject).
    #[serde(default)]
    pub cookies: Vec<Cookie>,
    /// Portable localStorage, per origin.
    #[serde(default, rename = "localStorage")]
    pub local_storage: Vec<OriginState>,
    /// Native-layer files (`IndexedDB` + service workers), destination-relative.
    #[serde(default)]
    pub indexeddb: Vec<ManifestEntry>,
}

impl ProfilePayload {
    /// The candidate origins to capture localStorage from: everything this
    /// payload injected plus every distinct `https://host` derived from its
    /// cookie domains. See `capture_local_storage`'s contract.
    #[must_use]
    pub fn candidate_origins(&self) -> Vec<String> {
        let mut origins: Vec<String> = self
            .local_storage
            .iter()
            .map(|o| o.origin.clone())
            .collect();
        for cookie in &self.cookies {
            let host = cookie.domain.trim_start_matches('.');
            if host.is_empty() {
                continue;
            }
            let origin = format!("https://{host}");
            if !origins.contains(&origin) {
                origins.push(origin);
            }
        }
        origins
    }

    /// True when the payload carries no state at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cookies.is_empty() && self.local_storage.is_empty() && self.indexeddb.is_empty()
    }
}

impl fmt::Debug for ProfilePayload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProfilePayload")
            .field("cookies", &self.cookies.len())
            .field("origins", &self.local_storage.len())
            .field("indexeddb_files", &self.indexeddb.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::cdp::StorageEntry;
    use crate::profile::cookie::SameSite;

    fn cookie(domain: &str) -> Cookie {
        Cookie {
            name: "sid".into(),
            value: "SECRET".into(),
            domain: domain.into(),
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
        }
    }

    #[test]
    fn debug_never_leaks_secret_values() {
        let payload = ProfilePayload {
            cookies: vec![cookie(".example.com")],
            local_storage: vec![OriginState {
                origin: "https://example.com".into(),
                local_storage: vec![StorageEntry {
                    name: "k".into(),
                    value: "SECRET".into(),
                }],
            }],
            indexeddb: vec![],
        };
        let rendered = format!("{payload:?}");
        assert!(
            !rendered.contains("SECRET"),
            "no secret in Debug: {rendered}"
        );
        assert!(rendered.contains("cookies: 1"));
        assert!(rendered.contains("origins: 1"));
    }

    #[test]
    fn candidate_origins_union_localstorage_and_cookie_hosts() {
        let payload = ProfilePayload {
            cookies: vec![cookie(".example.com"), cookie("app.other.com")],
            local_storage: vec![OriginState {
                origin: "https://example.com".into(),
                local_storage: vec![],
            }],
            indexeddb: vec![],
        };
        let origins = payload.candidate_origins();
        assert!(origins.contains(&"https://example.com".to_owned()));
        assert!(origins.contains(&"https://app.other.com".to_owned()));
        // .example.com and https://example.com must not double-count
        assert_eq!(
            origins
                .iter()
                .filter(|o| *o == "https://example.com")
                .count(),
            1
        );
    }

    #[test]
    fn deserializes_localstorage_camelcase_key() {
        let json = r#"{ "cookies": [], "localStorage": [], "indexeddb": [] }"#;
        let payload: ProfilePayload = serde_json::from_str(json).unwrap();
        assert!(payload.is_empty());
    }
}
