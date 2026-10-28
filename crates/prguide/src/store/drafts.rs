//! The `drafts` table: one row per `guide new`.

use super::StoreError;

pub struct NewDraft<'a> {
    pub id: &'a str,
    pub run_id: &'a str,
    pub path: &'a str,
    pub provenance_json: &'a str,
    pub created_at: &'a str,
}

/// Inserts a draft row. Call it inside the same transaction as the run gate.
pub fn insert(tx: &rusqlite::Transaction<'_>, draft: &NewDraft<'_>) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO drafts (id, run_id, path, provenance_json, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            draft.id,
            draft.run_id,
            draft.path,
            draft.provenance_json,
            draft.created_at
        ],
    )?;
    Ok(())
}
