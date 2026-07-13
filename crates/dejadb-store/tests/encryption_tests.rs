//! encryption-at-rest: AES-256-GCM via Turso's page cipher, wired through
//! `DejaDbOptions::encryption_key` / `DejaDB::open_encrypted`.
//! Proves round-trip with the key, and that a wrong/absent key is denied —
//! the crypto-erasure property (destroy the key ⇒ the memory is unreadable).

use dejadb_core::types::{Fact, Grain};
use dejadb_store::{DejaDB, DejaDbOptions};

fn enc_opts(key: [u8; 32]) -> DejaDbOptions {
    DejaDbOptions { index_text: false, encryption_key: Some(key), ..Default::default() }
}

fn fact(s: &str, r: &str, o: &str) -> Fact {
    let mut f = Fact::new(s, r, o).confidence(0.9);
    f.common.namespace = Some("main".into());
    f
}

/// db file + its -wal sibling, concatenated, for the plaintext-leak scan.
fn file_family_bytes(db: &std::path::Path) -> Vec<u8> {
    let mut out = std::fs::read(db).unwrap_or_default();
    let wal = db.with_file_name(format!("{}-wal", db.file_name().unwrap().to_string_lossy()));
    if let Ok(w) = std::fs::read(&wal) {
        out.extend_from_slice(&w);
    }
    out
}

#[test]
fn encryption_at_rest_roundtrip_and_key_denial() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("enc.db");
    let p = path.to_str().unwrap();
    let key = [7u8; 32];
    let marker = "SECRETMARKER-zzq-9931";

    // write encrypted
    {
        let mut m = DejaDB::open_with(p, enc_opts(key)).unwrap();
        m.add(&fact("alice", "medical_note", &format!("{marker}-penicillin-allergy"))).unwrap();
        assert_eq!(m.recall("main", "alice", None, 8).unwrap().len(), 1);
    }

    // the plaintext marker must NOT appear in the raw db/-wal bytes
    let raw = file_family_bytes(&path);
    let leaked = raw.windows(marker.len()).any(|w| w == marker.as_bytes());
    assert!(!leaked, "plaintext leaked in encrypted db file — cipher not effective");

    // reopen with the correct key → readable, content intact
    {
        let mut m = DejaDB::open_with(p, enc_opts(key)).unwrap();
        let r = m.recall("main", "alice", None, 8).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].get_str("object").unwrap().contains(marker));
    }

    // wrong key → denied
    assert!(DejaDB::open_with(p, enc_opts([9u8; 32])).is_err(), "wrong key must be rejected");
    // no key (bare open) → denied — destroy the key and the memory is gone
    assert!(DejaDB::open(p).is_err(), "opening an encrypted file without a key must fail");
}

#[test]
fn open_encrypted_convenience_roundtrips() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("enc2.db");
    let p = path.to_str().unwrap();
    let key = [42u8; 32];
    {
        let mut m = DejaDB::open_encrypted(p, key).unwrap();
        m.add(&fact("bob", "prefers", "window seat")).unwrap();
    }
    let mut m = DejaDB::open_encrypted(p, key).unwrap();
    assert_eq!(m.recall("main", "bob", None, 8).unwrap().len(), 1);
}
