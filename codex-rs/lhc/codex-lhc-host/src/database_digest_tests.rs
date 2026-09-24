use super::lhc_database_digest;
use lhc::sdk::OpResult;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

fn open(path: &std::path::Path) -> lhc::shared_tech::storage::Db {
    match lhc::shared_tech::storage::open_database(path.to_str().expect("utf-8")) {
        OpResult::Ok { value } => value,
        OpResult::Err { error } => panic!("{}", error.reason),
    }
}

fn seed(path: &std::path::Path) {
    let db = open(path);
    db.exec(
        r#"
        CREATE TABLE message (
          message_id TEXT PRIMARY KEY,
          turn_id TEXT NOT NULL,
          deleted_at TEXT
        );
        CREATE TABLE turns (
          turn_id TEXT PRIMARY KEY,
          status TEXT NOT NULL
        );
        CREATE TABLE derivation (
          subject_id TEXT PRIMARY KEY,
          content TEXT
        );
        CREATE TABLE thread_view (
          singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
          arrangement_json TEXT NOT NULL
        );
        INSERT INTO message (message_id, turn_id, deleted_at) VALUES ('m1', 't1', NULL);
        INSERT INTO turns (turn_id, status) VALUES ('t1', 'open');
        INSERT INTO derivation (subject_id, content) VALUES ('d1', 'summary');
        INSERT INTO thread_view (singleton, arrangement_json) VALUES (1, '[]');
        "#,
    );
    db.close();
}

#[test]
fn deleted_at_change_changes_the_digest() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("thread.sqlite");
    seed(&path);
    let before = lhc_database_digest(&path).expect("digest");
    let db = open(&path);
    db.exec("UPDATE message SET deleted_at = '2026-01-01T00:00:00.000Z' WHERE message_id = 'm1'");
    db.close();
    let after = lhc_database_digest(&path).expect("digest");
    assert_ne!(
        before, after,
        "Alder's counterexample: message.deleted_at must be in the digest"
    );
}

#[test]
fn turn_status_and_derived_view_changes_change_the_digest() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("thread.sqlite");
    seed(&path);
    let baseline = lhc_database_digest(&path).expect("digest");

    let db = open(&path);
    db.exec("UPDATE turns SET status = 'closed' WHERE turn_id = 't1'");
    db.close();
    let after_status = lhc_database_digest(&path).expect("digest");
    assert_ne!(baseline, after_status, "turns.status must be in the digest");

    let db = open(&path);
    db.exec("UPDATE derivation SET content = 'other' WHERE subject_id = 'd1'");
    db.close();
    let after_derivation = lhc_database_digest(&path).expect("digest");
    assert_ne!(
        after_status, after_derivation,
        "derived summaries must be in the digest"
    );

    let db = open(&path);
    db.exec("UPDATE thread_view SET arrangement_json = '[1]' WHERE singleton = 1");
    db.close();
    let after_view = lhc_database_digest(&path).expect("digest");
    assert_ne!(
        after_derivation, after_view,
        "thread_view contents must be in the digest"
    );
}

#[test]
fn digest_is_stable_across_repeated_wal_checkpoints() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("thread.sqlite");
    seed(&path);
    let first = lhc_database_digest(&path).expect("digest");
    let second = lhc_database_digest(&path).expect("digest");
    assert_eq!(first, second);
}
