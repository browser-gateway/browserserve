//! Reads a session's localStorage directly from Chrome's on-disk `LevelDB`.
//!
//! Runs after the browser is dead (`LevelDB` is copy-safe only then). Unlike the
//! CDP candidate-origin capture, this enumerates EVERY origin that wrote
//! localStorage, including cookieless ones. Format (Chrome 96+): a data record
//! has key `_<origin>\0<enc><key>` and value `<enc><string>`, where the encoding
//! byte is `0x01` for Latin-1 and `0x00` for UTF-16LE. `VERSION`/`META:` keys
//! are skipped.
//!
//! Verified against Chromium `local_storage_impl.cc` + `cached_storage_area.cc`
//! (`StorageFormat` 0=UTF16/1=Latin1) and CCL's forensic parser — see
//! `docs-internal/features/profile-localstorage/REFERENCES.md`. Partitioned
//! (third-party) keys serialize the whole `blink::StorageKey`
//! (`https://a.com/^0https://b.com`); those are NOT plain origins and cannot be
//! replayed as first-party, so they are skipped (detected by `^`).

use crate::profile::cdp::{OriginState, StorageEntry};
use rusty_leveldb::LdbIterator;
use std::collections::BTreeMap;
use std::path::Path;

/// Reads localStorage for every origin from the `LevelDB` at `leveldb_dir`.
/// Best-effort: returns an empty vec if the store is absent or unreadable.
#[must_use]
pub fn read_local_storage(leveldb_dir: &Path) -> Vec<OriginState> {
    if !leveldb_dir.is_dir() {
        return Vec::new();
    }
    let options = rusty_leveldb::Options::default();
    let Ok(mut db) = rusty_leveldb::DB::open(leveldb_dir, options) else {
        return Vec::new();
    };
    let Ok(mut iter) = db.new_iter() else {
        return Vec::new();
    };
    let mut by_origin: BTreeMap<String, Vec<StorageEntry>> = BTreeMap::new();
    while iter.advance() {
        let Some((key, value)) = iter.current() else {
            continue;
        };
        if let Some((origin, name)) = parse_data_key(&key)
            && let Some(decoded) = decode_string(&value)
        {
            by_origin.entry(origin).or_default().push(StorageEntry {
                name,
                value: decoded,
            });
        }
    }
    by_origin
        .into_iter()
        .map(|(origin, local_storage)| OriginState {
            origin,
            local_storage,
        })
        .collect()
}

// `_<origin>\0<enc><key>` -> (origin, decoded key). None for VERSION/META, and
// for partitioned StorageKeys (which contain `^` and are not first-party
// origins we can replay).
fn parse_data_key(key: &[u8]) -> Option<(String, String)> {
    let rest = key.strip_prefix(b"_")?;
    let nul = rest.iter().position(|&b| b == 0)?;
    let origin = std::str::from_utf8(&rest[..nul]).ok()?;
    if origin.contains('^') {
        return None;
    }
    let name = decode_string(&rest[nul + 1..])?;
    Some((origin.to_owned(), name))
}

// `<enc><bytes>`: enc 0x00 = UTF-16LE, 0x01 = Latin-1.
fn decode_string(encoded: &[u8]) -> Option<String> {
    let (encoding, bytes) = encoded.split_first()?;
    match encoding {
        0 => {
            if bytes.len() % 2 != 0 {
                return None;
            }
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            String::from_utf16(&units).ok()
        }
        1 => Some(bytes.iter().map(|&b| b as char).collect()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn latin1(s: &str) -> Vec<u8> {
        let mut out = vec![1u8];
        out.extend(s.bytes());
        out
    }

    #[test]
    fn parses_chrome_format_from_a_real_leveldb() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("leveldb");
        {
            let mut db = rusty_leveldb::DB::open(&dir, rusty_leveldb::Options::default()).unwrap();
            db.put(b"VERSION", b"1").unwrap();
            db.put(b"META:https://example.com", &[0, 1, 2]).unwrap();
            // _https://example.com\0<enc>probeKey  ->  <enc>probeValue
            let mut key = b"_https://example.com\0".to_vec();
            key.extend(latin1("probeKey"));
            db.put(&key, &latin1("probeValue")).unwrap();
            // a cookieless origin still gets captured
            let mut key2 = b"_https://cookieless.test\0".to_vec();
            key2.extend(latin1("k2"));
            db.put(&key2, &latin1("v2")).unwrap();
            // a PARTITIONED (third-party) StorageKey must be skipped, not
            // mislabeled as a first-party origin.
            let mut partitioned = b"_https://a.com/^0https://b.com\0".to_vec();
            partitioned.extend(latin1("pk"));
            db.put(&partitioned, &latin1("pv")).unwrap();
            db.flush().unwrap();
        }

        let origins = read_local_storage(&dir);
        let example = origins
            .iter()
            .find(|o| o.origin == "https://example.com")
            .unwrap();
        assert_eq!(
            example.local_storage,
            vec![StorageEntry {
                name: "probeKey".into(),
                value: "probeValue".into(),
            }]
        );
        // the origin with no cookie is enumerated too — the whole point
        assert!(
            origins
                .iter()
                .any(|o| o.origin == "https://cookieless.test")
        );
        // VERSION and META keys are not mistaken for data
        assert!(origins.iter().all(|o| !o.origin.contains("META")));
        // partitioned StorageKeys are skipped, never mislabeled as an origin
        assert!(origins.iter().all(|o| !o.origin.contains('^')));
    }

    #[test]
    fn decodes_utf16_values() {
        // enc 0x00 = UTF-16LE: "hi" = 68 00 69 00
        assert_eq!(decode_string(&[0, 0x68, 0, 0x69, 0]).as_deref(), Some("hi"));
    }

    #[test]
    fn missing_dir_is_empty() {
        assert!(read_local_storage(Path::new("/nonexistent/leveldb")).is_empty());
    }

    #[test]
    #[ignore = "needs a real Chrome Local Storage leveldb dir in LS_PROBE"]
    fn reads_a_real_chrome_leveldb() {
        let dir = std::env::var("LS_PROBE").expect("set LS_PROBE");
        let origins = read_local_storage(Path::new(&dir));
        eprintln!("{origins:#?}");
        assert!(
            origins.iter().any(|o| o.origin.contains("example.com")),
            "real Chrome leveldb should yield example.com",
        );
    }
}
