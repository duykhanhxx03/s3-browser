//! Durable record of the transfer queue.
//!
//! The queue survives a crash or a quit: jobs, their byte progress, the
//! multipart upload id and every part the server has already accepted are
//! written here as they happen, so a restart resumes instead of starting over.
//!
//! `rusqlite` is synchronous. Every statement here touches at most a handful of
//! rows and runs at most once per part, so the connection is guarded by a
//! `Mutex` rather than pushed onto a blocking pool.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use s3core::CompletedPart;

use crate::{Direction, Job, JobState};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS jobs (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    direction    TEXT    NOT NULL,
    local_path   TEXT    NOT NULL,
    bucket       TEXT    NOT NULL,
    key          TEXT    NOT NULL,
    size         INTEGER NOT NULL,
    transferred  INTEGER NOT NULL DEFAULT 0,
    state        TEXT    NOT NULL,
    upload_id    TEXT,
    etag         TEXT,
    error        TEXT,
    created_at   INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS parts (
    job_id      INTEGER NOT NULL REFERENCES jobs(id) ON DELETE CASCADE,
    part_number INTEGER NOT NULL,
    etag        TEXT    NOT NULL,
    size        INTEGER NOT NULL,
    PRIMARY KEY (job_id, part_number)
);

CREATE INDEX IF NOT EXISTS jobs_state ON jobs(state);
";

pub struct Journal {
    connection: Mutex<Connection>,
}

impl Journal {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let connection = Connection::open(path)
            .with_context(|| format!("opening transfer journal at {}", path.display()))?;
        Self::prepare(connection)
    }

    pub fn in_memory() -> Result<Self> {
        Self::prepare(Connection::open_in_memory()?)
    }

    fn prepare(connection: Connection) -> Result<Self> {
        // WAL keeps a reader (the UI snapshot) from blocking the writer (a part
        // finishing), and foreign keys make part rows vanish with their job.
        connection.pragma_update(None, "journal_mode", "WAL").ok();
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection
            .execute_batch(SCHEMA)
            .context("creating transfer journal schema")?;
        // Journals written before checksums existed have no such column. Adding
        // it here rather than in SCHEMA keeps `CREATE TABLE IF NOT EXISTS` from
        // silently skipping the change on an existing file; the error when it is
        // already present is the expected outcome, not a failure.
        _ = connection.execute("ALTER TABLE parts ADD COLUMN checksum_crc32 TEXT", []);
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn insert(&self, job: &Job) -> Result<i64> {
        let connection = self.connection.lock().unwrap();
        connection.execute(
            "INSERT INTO jobs (direction, local_path, bucket, key, size, transferred, state, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                job.direction.as_str(),
                job.local.to_string_lossy(),
                job.bucket,
                job.key,
                job.size as i64,
                job.transferred as i64,
                job.state.as_str(),
                job.created_at,
            ],
        )?;
        Ok(connection.last_insert_rowid())
    }

    pub fn set_state(&self, id: i64, state: JobState) -> Result<()> {
        self.connection.lock().unwrap().execute(
            "UPDATE jobs SET state = ?2, error = NULL WHERE id = ?1",
            params![id, state.as_str()],
        )?;
        Ok(())
    }

    pub fn set_failed(&self, id: i64, error: &str) -> Result<()> {
        self.connection.lock().unwrap().execute(
            "UPDATE jobs SET state = ?2, error = ?3 WHERE id = ?1",
            params![id, JobState::Failed.as_str(), error],
        )?;
        Ok(())
    }

    pub fn set_progress(&self, id: i64, transferred: u64) -> Result<()> {
        self.connection.lock().unwrap().execute(
            "UPDATE jobs SET transferred = ?2 WHERE id = ?1",
            params![id, transferred as i64],
        )?;
        Ok(())
    }

    pub fn set_upload_id(&self, id: i64, upload_id: &str) -> Result<()> {
        self.connection.lock().unwrap().execute(
            "UPDATE jobs SET upload_id = ?2 WHERE id = ?1",
            params![id, upload_id],
        )?;
        Ok(())
    }

    pub fn upload_id(&self, id: i64) -> Result<Option<String>> {
        let connection = self.connection.lock().unwrap();
        Ok(connection
            .query_row("SELECT upload_id FROM jobs WHERE id = ?1", params![id], |row| {
                row.get::<_, Option<String>>(0)
            })
            .optional()?
            .flatten())
    }

    pub fn set_etag(&self, id: i64, etag: Option<&str>) -> Result<()> {
        self.connection.lock().unwrap().execute(
            "UPDATE jobs SET etag = ?2 WHERE id = ?1",
            params![id, etag],
        )?;
        Ok(())
    }

    pub fn etag(&self, id: i64) -> Result<Option<String>> {
        let connection = self.connection.lock().unwrap();
        Ok(connection
            .query_row("SELECT etag FROM jobs WHERE id = ?1", params![id], |row| {
                row.get::<_, Option<String>>(0)
            })
            .optional()?
            .flatten())
    }

    pub fn record_part(&self, job_id: i64, part: &CompletedPart) -> Result<()> {
        self.connection.lock().unwrap().execute(
            "INSERT OR REPLACE INTO parts (job_id, part_number, etag, size, checksum_crc32) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                job_id,
                part.part_number,
                part.etag,
                part.size as i64,
                part.checksum_crc32
            ],
        )?;
        Ok(())
    }

    pub fn parts(&self, job_id: i64) -> Result<Vec<CompletedPart>> {
        let connection = self.connection.lock().unwrap();
        let mut statement = connection.prepare(
            "SELECT part_number, etag, size, checksum_crc32 FROM parts \
             WHERE job_id = ?1 ORDER BY part_number",
        )?;
        let parts = statement
            .query_map(params![job_id], |row| {
                Ok(CompletedPart {
                    part_number: row.get(0)?,
                    etag: row.get(1)?,
                    size: row.get::<_, i64>(2)? as u64,
                    checksum_crc32: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(parts)
    }

    pub fn clear_parts(&self, job_id: i64) -> Result<()> {
        self.connection
            .lock()
            .unwrap()
            .execute("DELETE FROM parts WHERE job_id = ?1", params![job_id])?;
        Ok(())
    }

    pub fn load_all(&self) -> Result<Vec<Job>> {
        let connection = self.connection.lock().unwrap();
        let mut statement = connection.prepare(
            "SELECT id, direction, local_path, bucket, key, size, transferred, state, error, created_at
             FROM jobs ORDER BY id",
        )?;
        let jobs = statement
            .query_map([], |row| {
                Ok(Job {
                    id: row.get(0)?,
                    direction: Direction::from_str(&row.get::<_, String>(1)?),
                    local: PathBuf::from(row.get::<_, String>(2)?),
                    bucket: row.get(3)?,
                    key: row.get(4)?,
                    size: row.get::<_, i64>(5)? as u64,
                    transferred: row.get::<_, i64>(6)? as u64,
                    state: JobState::from_str(&row.get::<_, String>(7)?),
                    error: row.get(8)?,
                    created_at: row.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(jobs)
    }

    /// Drops finished jobs. Anything unfinished stays so it can be resumed.
    pub fn clear_finished(&self) -> Result<usize> {
        let removed = self.connection.lock().unwrap().execute(
            "DELETE FROM jobs WHERE state IN (?1, ?2)",
            params![JobState::Done.as_str(), JobState::Canceled.as_str()],
        )?;
        Ok(removed)
    }

    pub fn remove(&self, id: i64) -> Result<()> {
        self.connection
            .lock()
            .unwrap()
            .execute("DELETE FROM jobs WHERE id = ?1", params![id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job() -> Job {
        Job {
            id: 0,
            direction: Direction::Upload,
            local: PathBuf::from("/tmp/photo.jpg"),
            bucket: "demo".into(),
            key: "photos/photo.jpg".into(),
            size: 1024,
            transferred: 0,
            state: JobState::Queued,
            error: None,
            created_at: 1_700_000_000,
        }
    }

    #[test]
    fn round_trips_a_job() {
        let journal = Journal::in_memory().unwrap();
        let id = journal.insert(&job()).unwrap();

        let loaded = journal.load_all().unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].id, id);
        assert_eq!(loaded[0].key, "photos/photo.jpg");
        assert_eq!(loaded[0].state, JobState::Queued);
        assert_eq!(loaded[0].local, PathBuf::from("/tmp/photo.jpg"));
    }

    #[test]
    fn progress_and_state_survive_a_reload() {
        let journal = Journal::in_memory().unwrap();
        let id = journal.insert(&job()).unwrap();

        journal.set_progress(id, 512).unwrap();
        journal.set_state(id, JobState::Paused).unwrap();

        let loaded = journal.load_all().unwrap();
        assert_eq!(loaded[0].transferred, 512);
        assert_eq!(loaded[0].state, JobState::Paused);
    }

    #[test]
    fn failure_records_its_message_and_clears_on_retry() {
        let journal = Journal::in_memory().unwrap();
        let id = journal.insert(&job()).unwrap();

        journal.set_failed(id, "connection reset").unwrap();
        let loaded = journal.load_all().unwrap();
        assert_eq!(loaded[0].state, JobState::Failed);
        assert_eq!(loaded[0].error.as_deref(), Some("connection reset"));

        // Re-queuing must not leave the old error behind to confuse the user.
        journal.set_state(id, JobState::Queued).unwrap();
        let loaded = journal.load_all().unwrap();
        assert_eq!(loaded[0].error, None);
    }

    #[test]
    fn parts_are_recorded_for_resume_and_replaced_not_duplicated() {
        let journal = Journal::in_memory().unwrap();
        let id = journal.insert(&job()).unwrap();

        journal.set_upload_id(id, "upload-123").unwrap();
        assert_eq!(journal.upload_id(id).unwrap().as_deref(), Some("upload-123"));

        for number in [2, 1] {
            journal
                .record_part(
                    id,
                    &CompletedPart {
                        part_number: number,
                        etag: format!("etag-{number}"),
                        size: 5 * 1024 * 1024,
                        checksum_crc32: Some("y/Q5Jg==".into()),
                    },
                )
                .unwrap();
        }
        // A retried part must overwrite, not add a second row.
        journal
            .record_part(
                id,
                &CompletedPart {
                    part_number: 1,
                    etag: "etag-1-retry".into(),
                    size: 5 * 1024 * 1024,
                    checksum_crc32: Some("y/Q5Jg==".into()),
                },
            )
            .unwrap();

        let parts = journal.parts(id).unwrap();
        assert_eq!(parts.len(), 2, "got {parts:?}");
        assert_eq!(parts[0].part_number, 1, "parts come back in order");
        assert_eq!(parts[0].etag, "etag-1-retry");

        journal.clear_parts(id).unwrap();
        assert!(journal.parts(id).unwrap().is_empty());
    }

    #[test]
    fn clearing_finished_keeps_unfinished_work() {
        let journal = Journal::in_memory().unwrap();
        let done = journal.insert(&job()).unwrap();
        let paused = journal.insert(&job()).unwrap();
        let failed = journal.insert(&job()).unwrap();

        journal.set_state(done, JobState::Done).unwrap();
        journal.set_state(paused, JobState::Paused).unwrap();
        journal.set_failed(failed, "timeout").unwrap();

        assert_eq!(journal.clear_finished().unwrap(), 1);

        let remaining: Vec<_> = journal.load_all().unwrap();
        assert_eq!(remaining.len(), 2);
        assert!(
            remaining.iter().all(|job| job.id != done),
            "finished job should be gone"
        );
        assert!(
            remaining.iter().any(|job| job.state == JobState::Failed),
            "a failed job stays so it can be retried"
        );
    }

    #[test]
    fn deleting_a_job_takes_its_parts_with_it() {
        let journal = Journal::in_memory().unwrap();
        let id = journal.insert(&job()).unwrap();
        journal
            .record_part(
                id,
                &CompletedPart {
                    part_number: 1,
                    etag: "e".into(),
                    size: 1,
                    checksum_crc32: Some("y/Q5Jg==".into()),
                },
            )
            .unwrap();

        journal.remove(id).unwrap();
        assert!(
            journal.parts(id).unwrap().is_empty(),
            "orphaned part rows would leak forever"
        );
    }
}
