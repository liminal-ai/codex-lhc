//! Scoped server shutdown must not take another live owner's claims.

use super::*;
use lhc::shared_tech::errors::OpResult;
use lhc::shared_tech::storage::Db;
use lhc::shared_tech::storage::open_database;
use lhc::shared_tech::work_queue::ClaimAttempt;
use lhc::shared_tech::work_queue::note_claim_held;
use pretty_assertions::assert_eq;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn unique_thread_id(label: &str) -> String {
    format!(
        "{label}-{}-{}",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

fn open(path: &str) -> Db {
    match open_database(path) {
        OpResult::Ok { value } => value,
        OpResult::Err { error } => panic!("{}", error.reason),
    }
}

fn seed_claimed(db: &Db, work_item_id: &str) {
    db.exec(&format!(
        "CREATE TABLE IF NOT EXISTS work_item (
            work_item_id TEXT PRIMARY KEY,
            status TEXT,
            claimed_at TEXT,
            claim_expires_at TEXT,
            payload TEXT
         );
         INSERT INTO work_item VALUES ('{work_item_id}','claimed','now','later','{{\"claimAttempt\":1}}');"
    ));
    note_claim_held(
        db,
        &ClaimAttempt {
            work_item_id: work_item_id.into(),
            claim_attempt: Some(1),
        },
    );
}

fn status(db: &Db, work_item_id: &str) -> String {
    db.prepare("SELECT status FROM work_item WHERE work_item_id = ?")
        .get_params(&[lhc::shared_tech::storage::SqlParam::from(work_item_id)])
        .expect("row")
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

#[test]
fn path_scoped_unload_releases_only_that_thread_database() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    std::fs::create_dir_all(root.join("threads")).expect("threads dir");
    let client_a = unique_thread_id("client-a");
    let client_b = unique_thread_id("client-b");
    let path_a = thread_file_path(root, &client_a);
    let path_b = thread_file_path(root, &client_b);
    let db_a = open(path_a.to_str().expect("utf-8"));
    let db_b = open(path_b.to_str().expect("utf-8"));
    seed_claimed(&db_a, "w-a");
    seed_claimed(&db_b, "w-b");

    on_thread_unload(Some(root), &client_a);

    assert_eq!(status(&db_a, "w-a"), "queued");
    assert_eq!(status(&db_b, "w-b"), "claimed");

    on_thread_unload(Some(root), &client_b);
    assert_eq!(status(&db_b, "w-b"), "queued");
}
