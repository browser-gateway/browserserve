//! Reads a session's localStorage directly from Chrome's on-disk store.
//!
//! Chrome 153+ keeps localStorage in a `SQLite` file (`Default/LocalStorage`):
//! table `maps` holds one row per serialized `blink::StorageKey`, and
//! `map_entries(map_id, value_compression_type, key, value)` holds the pairs,
//! with values optionally compressed (0 none, 1 zstd, 2 snappy; Chromium
//! `components/services/storage/public/cpp/compression.h`). Keys and values keep
//! the `<enc><bytes>` encoding described below. Older Chrome uses `LevelDB`.
//!
//! Runs after the browser is dead (the stores are copy-safe only then). Unlike the
//! CDP candidate-origin capture, this enumerates EVERY origin that wrote
//! localStorage, including cookieless ones. Format (Chrome 96+): a data record
//! has key `_<origin>\0<enc><key>` and value `<enc><string>`, where the encoding
//! byte is `0x01` for Latin-1 and `0x00` for UTF-16LE. `VERSION`/`META:` keys
//! are skipped.
//!
//! Verified against Chromium `local_storage_impl.cc` + `cached_storage_area.cc`
//! (`StorageFormat` 0=UTF16/1=Latin1) and CCL's forensic parser. Partitioned
//! (third-party) keys serialize the whole `blink::StorageKey`
//! (`https://a.com/^0https://b.com`); those are NOT plain origins and cannot be
//! replayed as first-party, so they are skipped (detected by `^`).

use crate::profile::cdp::{OriginState, StorageEntry};
use rusty_leveldb::LdbIterator;
use std::collections::BTreeMap;
use std::path::Path;

/// Reads localStorage for every origin from a Chrome profile's `Default` dir,
/// using the `SQLite` store when present and the `LevelDB` store otherwise.
#[must_use]
pub fn read_profile_local_storage(default_dir: &Path) -> Vec<OriginState> {
    let sqlite = default_dir.join("LocalStorage");
    if sqlite.is_file() {
        return read_local_storage_sqlite(&sqlite);
    }
    read_local_storage(&default_dir.join("Local Storage/leveldb"))
}

/// Reads localStorage for every origin from the `SQLite` store at `db_path`.
/// Best-effort: returns an empty vec if the store is absent or unreadable.
#[must_use]
pub fn read_local_storage_sqlite(db_path: &Path) -> Vec<OriginState> {
    match read_sqlite(db_path) {
        Ok(origins) => origins,
        Err(e) => {
            tracing::warn!(error = %e, "localStorage sqlite read failed");
            Vec::new()
        }
    }
}

fn read_sqlite(db_path: &Path) -> rusqlite::Result<Vec<OriginState>> {
    let conn = rusqlite::Connection::open(db_path)?;
    let mut statement = conn.prepare(
        "SELECT maps.storage_key, map_entries.key, map_entries.value_compression_type, \
         map_entries.value FROM map_entries JOIN maps ON maps.row_id = map_entries.map_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Vec<u8>>(3)?,
        ))
    })?;
    let mut by_origin: BTreeMap<String, Vec<StorageEntry>> = BTreeMap::new();
    for row in rows {
        let (storage_key, key, compression, value) = row?;
        let Some(origin) = origin_from_storage_key(&storage_key) else {
            continue;
        };
        let Some(raw_value) = decompress(compression, value) else {
            continue;
        };
        if let (Some(name), Some(decoded)) = (decode_string(&key), decode_string(&raw_value)) {
            by_origin.entry(origin).or_default().push(StorageEntry {
                name,
                value: decoded,
            });
        }
    }
    Ok(by_origin
        .into_iter()
        .map(|(origin, local_storage)| OriginState {
            origin,
            local_storage,
        })
        .collect())
}

// A first-party StorageKey serializes as the origin URL (`https://a.com/`).
// Partitioned and opaque keys carry `^` and cannot be replayed first-party.
fn origin_from_storage_key(storage_key: &[u8]) -> Option<String> {
    let serialized = std::str::from_utf8(storage_key).ok()?;
    if serialized.contains('^') {
        return None;
    }
    Some(
        serialized
            .strip_suffix('/')
            .unwrap_or(serialized)
            .to_owned(),
    )
}

fn decompress(compression: i64, value: Vec<u8>) -> Option<Vec<u8>> {
    match compression {
        0 => Some(value),
        1 => {
            let mut decoder = ruzstd::decoding::StreamingDecoder::new(value.as_slice()).ok()?;
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut decoder, &mut out).ok()?;
            Some(out)
        }
        2 => snap::raw::Decoder::new().decompress_vec(&value).ok(),
        _ => None,
    }
}

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

    fn write_sqlite_store(path: &Path, rows: &[(&str, &str, i64, Vec<u8>)]) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE maps(row_id INTEGER PRIMARY KEY AUTOINCREMENT, storage_key BLOB NOT NULL, \
             last_accessed INTEGER, last_modified INTEGER, total_size INTEGER); \
             CREATE TABLE map_entries(map_id INTEGER NOT NULL, value_compression_type INTEGER NOT NULL, \
             key BLOB NOT NULL, value BLOB NOT NULL, PRIMARY KEY(map_id, key)) WITHOUT ROWID;",
        )
        .unwrap();
        for (storage_key, key, compression, value) in rows {
            conn.execute(
                "INSERT OR IGNORE INTO maps(storage_key) VALUES (?1)",
                [storage_key.as_bytes()],
            )
            .unwrap();
            let map_id: i64 = conn
                .query_row(
                    "SELECT row_id FROM maps WHERE storage_key = ?1",
                    [storage_key.as_bytes()],
                    |r| r.get(0),
                )
                .unwrap();
            conn.execute(
                "INSERT INTO map_entries VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![map_id, compression, latin1(key), value],
            )
            .unwrap();
        }
    }

    #[test]
    fn parses_chrome_sqlite_store_with_every_compression() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("LocalStorage");
        let big = "x".repeat(600);
        let snappy = snap::raw::Encoder::new()
            .compress_vec(&latin1(&big))
            .unwrap();
        write_sqlite_store(
            &path,
            &[
                ("https://example.com/", "plain", 0, latin1("v1")),
                ("https://example.com/", "snappy", 2, snappy),
                ("https://cookieless.test/", "k2", 0, latin1("v2")),
                ("https://a.com/^0https://b.com", "pk", 0, latin1("pv")),
                ("https://example.com/", "unknown", 9, latin1("??")),
            ],
        );
        let origins = read_local_storage_sqlite(&path);
        let example = origins
            .iter()
            .find(|o| o.origin == "https://example.com")
            .unwrap();
        assert!(example.local_storage.contains(&StorageEntry {
            name: "plain".into(),
            value: "v1".into(),
        }));
        assert!(example.local_storage.contains(&StorageEntry {
            name: "snappy".into(),
            value: big.clone(),
        }));
        assert!(!example.local_storage.iter().any(|e| e.name == "unknown"));
        assert!(
            origins
                .iter()
                .any(|o| o.origin == "https://cookieless.test")
        );
        assert!(origins.iter().all(|o| !o.origin.contains('^')));
    }

    #[test]
    fn decodes_a_zstd_value_written_by_chrome() {
        // Value of localStorage "big" = "abc" x 500, as stored by Chrome 153.
        let stored = hex_bytes("28B52FFD60DD0475000038016162636162630100D3E52F46");
        let raw = decompress(1, stored).unwrap();
        assert_eq!(decode_string(&raw).unwrap(), "abc".repeat(500));
    }

    fn hex_bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn profile_reader_prefers_sqlite_over_leveldb() {
        let tmp = tempfile::tempdir().unwrap();
        write_sqlite_store(
            &tmp.path().join("LocalStorage"),
            &[("https://new.test/", "k", 0, latin1("from-sqlite"))],
        );
        let origins = read_profile_local_storage(tmp.path());
        assert_eq!(origins.len(), 1);
        assert_eq!(origins[0].local_storage[0].value, "from-sqlite");
        assert!(read_profile_local_storage(&tmp.path().join("missing")).is_empty());
    }

    #[test]
    fn missing_dir_is_empty() {
        assert!(read_local_storage(Path::new("/nonexistent/leveldb")).is_empty());
    }

    #[test]
    #[ignore = "needs a real Chrome 153+ LocalStorage sqlite file in LS_SQLITE_PROBE"]
    fn reads_a_real_chrome_sqlite_store() {
        let path = std::env::var("LS_SQLITE_PROBE").expect("set LS_SQLITE_PROBE");
        let origins = read_local_storage_sqlite(Path::new(&path));
        eprintln!("{origins:#?}");
        assert!(
            !origins.is_empty(),
            "real Chrome sqlite store should yield origins"
        );
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
