//! Whole-database digest of a quiesced LHC thread SQLite file.
//!
//! Callers must stop capture writers first. This module checkpoints the WAL,
//! then hashes sqlite_master (schema, including views/indexes) and every table
//! listed there, all columns, ordered by primary key or rowid.

use std::path::Path;

use lhc::sdk::OpResult;
use sha2::Digest;
use sha2::Sha256;

/// Checkpoint the WAL, then digest every table in `sqlite_master`.
///
/// Fails if the database is missing or unreadable. Capture must already be
/// quiesced; this only checkpoints leftover WAL frames.
pub fn lhc_database_digest(path: &Path) -> Result<String, String> {
    let Some(path) = path.to_str() else {
        return Err("LHC database path is not utf-8".into());
    };
    if !std::path::Path::new(path).is_file() {
        return Err(format!("LHC database missing: {path}"));
    }
    let db = match lhc::shared_tech::storage::open_database(path) {
        OpResult::Ok { value } => value,
        OpResult::Err { error } => return Err(error.reason),
    };
    db.exec("PRAGMA wal_checkpoint(TRUNCATE)");
    let mut hasher = Sha256::new();
    for row in db
        .prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name")
        .all(&[])
    {
        hash_sql_row(&mut hasher, &row);
    }
    let tables: Vec<String> = db
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .all(&[])
        .into_iter()
        .filter_map(|row| {
            row.get("name")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        })
        .collect();
    for table in tables {
        hasher.update(b"table:");
        hasher.update(table.as_bytes());
        hasher.update([0]);
        let sql = select_all_ordered(&db, &table);
        for row in db.prepare(&sql).all(&[]) {
            hash_sql_row(&mut hasher, &row);
        }
    }
    db.close();
    Ok(format!("{:x}", hasher.finalize()))
}

fn select_all_ordered(db: &lhc::shared_tech::storage::Db, table: &str) -> String {
    let quoted = quote_ident(table);
    let mut pks: Vec<(i64, String)> = db
        .prepare(&format!("PRAGMA table_info({quoted})"))
        .all(&[])
        .into_iter()
        .filter_map(|row| {
            let pk = row.get("pk").and_then(json_i64).unwrap_or(0);
            if pk <= 0 {
                return None;
            }
            let name = row.get("name")?.as_str()?;
            Some((pk, quote_ident(name)))
        })
        .collect();
    pks.sort_by_key(|(pk, _)| *pk);
    if pks.is_empty() {
        format!("SELECT * FROM {quoted} ORDER BY rowid")
    } else {
        let cols = pks
            .iter()
            .map(|(_, name)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        format!("SELECT * FROM {quoted} ORDER BY {cols}")
    }
}

fn json_i64(value: &serde_json::Value) -> Option<i64> {
    match value {
        serde_json::Value::Number(number) => number.as_i64(),
        _ => None,
    }
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn hash_sql_row(hasher: &mut Sha256, row: &serde_json::Map<String, serde_json::Value>) {
    let mut keys: Vec<_> = row.keys().cloned().collect();
    keys.sort();
    for key in keys {
        hasher.update(key.as_bytes());
        hasher.update([0]);
        match row.get(&key) {
            Some(value) => hasher.update(value.to_string().as_bytes()),
            None => hasher.update(b"<null>"),
        }
        hasher.update([0xff]);
    }
}

#[cfg(test)]
#[path = "database_digest_tests.rs"]
mod tests;
