//! Cookie model and the pre-inject sanitizer.
//!
//! Cookies are auth material. The one rule that matters: every security
//! attribute a browser set (`secure`, `httpOnly`, `sameSite`, `partitionKey`,
//! schemeful source) must survive the capture -> store -> re-inject round trip,
//! or a protected session cookie silently degrades into one an attacker can
//! read or replay. So the model carries the COMPLETE CDP field set and the
//! sanitizer only ever DROPS a cookie it cannot faithfully reproduce; it never
//! rewrites `secure`/`httpOnly`/`sameSite`/`domain` to make one "fit".
//!
//! Field set and drop rules are grounded in Chromium `canonical_cookie.cc`
//! (`CreateSanitizedCookie`, the `__Host-`/`__Secure-` prefix rules, the
//! SameSite=None-requires-Secure exclusion) — see
//! `docs-internal/features/profile-preseed/` D7.

use serde::{Deserialize, Serialize};

/// Cross-site scoping key of a partitioned (CHIPS) cookie.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PartitionKey {
    /// The top-level site the cookie is partitioned under.
    pub top_level_site: String,
    /// Whether a cross-site ancestor is present in the frame chain.
    pub has_cross_site_ancestor: bool,
}

/// `SameSite` policy. Absent means Chrome's UNSPECIFIED (~Lax); we re-inject it
/// as absent, which reproduces UNSPECIFIED faithfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SameSite {
    /// Sent only for same-site requests.
    Strict,
    /// Sent for same-site and top-level cross-site navigations.
    Lax,
    /// Sent cross-site; Chrome requires `secure=true` or it is excluded.
    None,
}

/// A captured cookie: exactly what `Storage.getCookies` returns, so nothing an
/// attribute-bearing browser set is lost on the way to storage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cookie {
    /// Cookie name (may carry a `__Host-`/`__Secure-` prefix).
    pub name: String,
    /// Cookie value — a bearer secret; never log it.
    pub value: String,
    /// Scope host. A leading dot marks a domain (non-host-only) cookie; keep it
    /// verbatim — adding or removing it re-scopes the cookie.
    pub domain: String,
    /// Scope path.
    pub path: String,
    /// Seconds since epoch, or `-1` for a session cookie.
    #[serde(default = "session_expiry")]
    pub expires: f64,
    /// Not sent over plaintext HTTP when true.
    #[serde(default)]
    pub secure: bool,
    /// Not exposed to JavaScript when true.
    #[serde(default)]
    pub http_only: bool,
    /// `SameSite` policy; absent = `UNSPECIFIED`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_site: Option<SameSite>,
    /// Eviction priority (`Low`/`Medium`/`High`); pass-through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// Schemeful same-site source scheme (`Unset`/`NonSecure`/`Secure`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_scheme: Option<String>,
    /// Source port (`-1` = unspecified); pass-through.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_port: Option<i64>,
    /// Partition (CHIPS) key, if partitioned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partition_key: Option<PartitionKey>,
    /// True when the partition key is opaque and NOT serializable: such a cookie
    /// cannot be faithfully re-injected and is dropped.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub partition_key_opaque: bool,
}

fn session_expiry() -> f64 {
    -1.0
}

/// A cookie ready for `Storage.setCookies`: the `Network.CookieParam` shape.
/// `expires` is omitted for session cookies; `size`/`session`/opaque-partition
/// are not set-params and are absent by construction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CookieParam {
    /// Cookie name.
    pub name: String,
    /// Cookie value.
    pub value: String,
    /// Scope host (verbatim, leading dot preserved).
    pub domain: String,
    /// Scope path.
    pub path: String,
    /// Seconds since epoch; absent for a session cookie.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires: Option<f64>,
    /// Secure attribute.
    pub secure: bool,
    /// `HttpOnly` attribute.
    pub http_only: bool,
    /// `SameSite` policy; absent = `UNSPECIFIED`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub same_site: Option<SameSite>,
    /// Eviction priority; pass-through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    /// Schemeful source scheme; pass-through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_scheme: Option<String>,
    /// Source port; pass-through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_port: Option<i64>,
    /// Partition (CHIPS) key; absent = unpartitioned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partition_key: Option<PartitionKey>,
}

/// Why a cookie was dropped, counted for logging without exposing any value.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DropCounts {
    /// `SameSite=None` without `secure` (Chrome would exclude it anyway).
    pub same_site_none_insecure: u32,
    /// Persistent cookie already past its expiry.
    pub expired: u32,
    /// Opaque partition key — not serializable, unsafe to re-inject.
    pub opaque_partition: u32,
    /// `__Host-`/`__Secure-` prefix constraints violated (corrupt input).
    pub invalid_prefix: u32,
    /// Name+value over the ~4 KB per-cookie ceiling.
    pub oversized: u32,
}

impl DropCounts {
    /// Total number of cookies dropped across all reasons.
    #[must_use]
    pub fn total(&self) -> u32 {
        self.same_site_none_insecure
            + self.expired
            + self.opaque_partition
            + self.invalid_prefix
            + self.oversized
    }
}

/// Approximate per-cookie byte ceiling (name + value); Chrome rejects ~4 KB.
const MAX_COOKIE_BYTES: usize = 4096;

/// Turns captured cookies into a safe `setCookies` payload, dropping only those
/// that cannot be faithfully and safely reproduced. `now_secs` is the current
/// time (seconds since epoch) used for the expiry check.
///
/// Every retained cookie carries all of its original security attributes
/// verbatim. One malformed cookie never aborts the batch.
#[must_use]
pub fn sanitize(cookies: &[Cookie], now_secs: f64) -> (Vec<CookieParam>, DropCounts) {
    let mut params = Vec::with_capacity(cookies.len());
    let mut dropped = DropCounts::default();
    for cookie in cookies {
        match to_param(cookie, now_secs) {
            Ok(param) => params.push(param),
            Err(reason) => count_drop(&mut dropped, reason),
        }
    }
    (params, dropped)
}

#[derive(Clone, Copy)]
enum DropReason {
    SameSiteNoneInsecure,
    Expired,
    OpaquePartition,
    InvalidPrefix,
    Oversized,
}

fn count_drop(counts: &mut DropCounts, reason: DropReason) {
    match reason {
        DropReason::SameSiteNoneInsecure => counts.same_site_none_insecure += 1,
        DropReason::Expired => counts.expired += 1,
        DropReason::OpaquePartition => counts.opaque_partition += 1,
        DropReason::InvalidPrefix => counts.invalid_prefix += 1,
        DropReason::Oversized => counts.oversized += 1,
    }
}

fn to_param(cookie: &Cookie, now_secs: f64) -> Result<CookieParam, DropReason> {
    if cookie.partition_key_opaque {
        return Err(DropReason::OpaquePartition);
    }
    if cookie.same_site == Some(SameSite::None) && !cookie.secure {
        return Err(DropReason::SameSiteNoneInsecure);
    }
    let is_session = cookie.expires < 0.0;
    if !is_session && cookie.expires < now_secs {
        return Err(DropReason::Expired);
    }
    if !prefix_ok(cookie) {
        return Err(DropReason::InvalidPrefix);
    }
    if cookie.name.len() + cookie.value.len() > MAX_COOKIE_BYTES {
        return Err(DropReason::Oversized);
    }
    Ok(CookieParam {
        name: cookie.name.clone(),
        value: cookie.value.clone(),
        domain: cookie.domain.clone(),
        path: cookie.path.clone(),
        expires: if is_session {
            None
        } else {
            Some(cookie.expires)
        },
        secure: cookie.secure,
        http_only: cookie.http_only,
        same_site: cookie.same_site,
        priority: cookie.priority.clone(),
        source_scheme: cookie.source_scheme.clone(),
        source_port: cookie.source_port,
        partition_key: cookie.partition_key.clone(),
    })
}

// `__Host-` requires secure + path "/" + host-only (no leading-dot domain);
// `__Secure-` requires secure. A captured cookie that violates these is corrupt
// and would fail the whole `setCookies` batch, so drop it.
fn prefix_ok(cookie: &Cookie) -> bool {
    if let Some(rest) = cookie.name.strip_prefix("__Host-") {
        return !rest.is_empty()
            && cookie.secure
            && cookie.path == "/"
            && !cookie.domain.starts_with('.');
    }
    if let Some(rest) = cookie.name.strip_prefix("__Secure-") {
        return !rest.is_empty() && cookie.secure;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Cookie {
        Cookie {
            name: "sid".into(),
            value: "secret".into(),
            domain: "example.com".into(),
            path: "/".into(),
            expires: -1.0,
            secure: true,
            http_only: true,
            same_site: Some(SameSite::Lax),
            priority: Some("Medium".into()),
            source_scheme: Some("Secure".into()),
            source_port: Some(443),
            partition_key: None,
            partition_key_opaque: false,
        }
    }

    #[test]
    fn preserves_every_security_attribute_verbatim() {
        let (params, dropped) = sanitize(&[base()], 1000.0);
        assert_eq!(dropped.total(), 0);
        let p = &params[0];
        assert!(p.secure);
        assert!(p.http_only);
        assert_eq!(p.same_site, Some(SameSite::Lax));
        assert_eq!(p.source_scheme.as_deref(), Some("Secure"));
        assert_eq!(p.source_port, Some(443));
        assert_eq!(p.priority.as_deref(), Some("Medium"));
        assert_eq!(p.expires, None, "session cookie -> no expires");
    }

    #[test]
    fn partition_key_round_trips() {
        let mut c = base();
        c.partition_key = Some(PartitionKey {
            top_level_site: "https://top.example".into(),
            has_cross_site_ancestor: true,
        });
        let (params, _) = sanitize(&[c], 1000.0);
        assert_eq!(
            params[0].partition_key,
            Some(PartitionKey {
                top_level_site: "https://top.example".into(),
                has_cross_site_ancestor: true,
            })
        );
    }

    #[test]
    fn drops_samesite_none_without_secure() {
        let mut c = base();
        c.same_site = Some(SameSite::None);
        c.secure = false;
        let (params, dropped) = sanitize(&[c], 1000.0);
        assert!(params.is_empty());
        assert_eq!(dropped.same_site_none_insecure, 1);
    }

    #[test]
    fn keeps_samesite_none_when_secure() {
        let mut c = base();
        c.same_site = Some(SameSite::None);
        c.secure = true;
        let (params, _) = sanitize(&[c], 1000.0);
        assert_eq!(params.len(), 1);
    }

    #[test]
    fn drops_expired_persistent_but_keeps_session_and_future() {
        let mut expired = base();
        expired.expires = 500.0;
        let mut future = base();
        future.expires = 5000.0;
        let session = base(); // expires -1
        let (params, dropped) = sanitize(&[expired, future, session], 1000.0);
        assert_eq!(dropped.expired, 1);
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn drops_opaque_partition() {
        let mut c = base();
        c.partition_key_opaque = true;
        let (params, dropped) = sanitize(&[c], 1000.0);
        assert!(params.is_empty());
        assert_eq!(dropped.opaque_partition, 1);
    }

    #[test]
    fn host_prefix_requires_secure_root_hostonly() {
        let mut ok = base();
        ok.name = "__Host-sid".into(); // secure, path "/", no dot -> ok
        let (p_ok, _) = sanitize(&[ok], 1000.0);
        assert_eq!(p_ok.len(), 1);

        for bad in [
            {
                let mut c = base();
                c.name = "__Host-sid".into();
                c.secure = false;
                c
            },
            {
                let mut c = base();
                c.name = "__Host-sid".into();
                c.path = "/app".into();
                c
            },
            {
                let mut c = base();
                c.name = "__Host-sid".into();
                c.domain = ".example.com".into();
                c
            },
        ] {
            let (p, dropped) = sanitize(&[bad], 1000.0);
            assert!(p.is_empty());
            assert_eq!(dropped.invalid_prefix, 1);
        }
    }

    #[test]
    fn secure_prefix_requires_secure() {
        let mut c = base();
        c.name = "__Secure-sid".into();
        c.secure = false;
        let (params, dropped) = sanitize(&[c], 1000.0);
        assert!(params.is_empty());
        assert_eq!(dropped.invalid_prefix, 1);
    }

    #[test]
    fn drops_oversized_without_aborting_batch() {
        let mut big = base();
        big.value = "x".repeat(MAX_COOKIE_BYTES);
        let good = base();
        let (params, dropped) = sanitize(&[big, good], 1000.0);
        assert_eq!(dropped.oversized, 1);
        assert_eq!(params.len(), 1, "one bad cookie must not drop the good one");
    }

    #[test]
    fn never_rewrites_attributes_to_force_a_fit() {
        // An insecure Lax cookie is valid and must pass through UNCHANGED —
        // the sanitizer must not "upgrade" secure to make anything fit.
        let mut c = base();
        c.secure = false;
        c.http_only = false;
        c.same_site = Some(SameSite::Lax);
        let (params, _) = sanitize(&[c], 1000.0);
        assert_eq!(params.len(), 1);
        assert!(!params[0].secure);
        assert!(!params[0].http_only);
    }

    #[test]
    fn cookie_json_uses_cdp_camelcase_field_names() {
        let json = serde_json::to_string(&base()).unwrap();
        assert!(json.contains("\"httpOnly\""));
        assert!(json.contains("\"sameSite\""));
        assert!(json.contains("\"sourceScheme\""));
        assert!(!json.contains("\"http_only\""));
    }

    #[test]
    fn param_omits_expires_for_session_cookie() {
        let (params, _) = sanitize(&[base()], 1000.0);
        let json = serde_json::to_string(&params[0]).unwrap();
        assert!(
            !json.contains("expires"),
            "session cookie param has no expires"
        );
    }
}
