//! Transfer queue: uploads and downloads with progress, pause, resume and cancel.
//!
//! The AWS Rust SDK's high-level transfer manager is still a developer preview
//! with no pause/resume, so multipart is driven here: parts go up concurrently,
//! each acceptance is journalled, and a resumed job asks the server which parts
//! it already holds instead of re-sending them.
//!
//! Nothing in this crate knows about GPUI. The UI drives it through
//! [`TransferEngine::snapshot`] and the control methods.

mod journal;
mod worker;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use s3core::S3Client;
use tokio::sync::Semaphore;

use journal::Journal;

/// Objects at or above this size go up in parts.
pub const MULTIPART_THRESHOLD: u64 = 16 * 1024 * 1024;
/// S3 rejects parts below 5 MiB (except the last one).
pub const MIN_PART_SIZE: u64 = 5 * 1024 * 1024;
const DEFAULT_PART_SIZE: u64 = 16 * 1024 * 1024;
/// S3 allows at most 10,000 parts per upload.
const MAX_PARTS: u64 = 10_000;

/// How many files move at once, and how many parts within one file.
const DEFAULT_JOB_CONCURRENCY: usize = 3;
const DEFAULT_PART_CONCURRENCY: usize = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Upload,
    Download,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Direction::Upload => "upload",
            Direction::Download => "download",
        }
    }

    fn from_str(text: &str) -> Self {
        match text {
            "download" => Direction::Download,
            _ => Direction::Upload,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Running,
    Paused,
    Done,
    Failed,
    Canceled,
}

impl JobState {
    fn as_str(self) -> &'static str {
        match self {
            JobState::Queued => "queued",
            JobState::Running => "running",
            JobState::Paused => "paused",
            JobState::Done => "done",
            JobState::Failed => "failed",
            JobState::Canceled => "canceled",
        }
    }

    fn from_str(text: &str) -> Self {
        match text {
            "running" => JobState::Running,
            "paused" => JobState::Paused,
            "done" => JobState::Done,
            "failed" => JobState::Failed,
            "canceled" => JobState::Canceled,
            _ => JobState::Queued,
        }
    }

    pub fn is_finished(self) -> bool {
        matches!(self, JobState::Done | JobState::Canceled)
    }

    pub fn is_active(self) -> bool {
        matches!(self, JobState::Queued | JobState::Running)
    }
}

#[derive(Clone, Debug)]
pub struct Job {
    pub id: i64,
    pub direction: Direction,
    pub local: PathBuf,
    pub bucket: String,
    pub key: String,
    pub size: u64,
    pub transferred: u64,
    pub state: JobState,
    pub error: Option<String>,
    pub created_at: i64,
}

impl Job {
    /// A name short enough for a queue row.
    pub fn display_name(&self) -> String {
        match self.direction {
            Direction::Upload => self
                .local
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| self.key.clone()),
            Direction::Download => self
                .key
                .rsplit('/')
                .next()
                .unwrap_or(&self.key)
                .to_string(),
        }
    }

    pub fn fraction(&self) -> f32 {
        if self.size == 0 {
            return if self.state == JobState::Done { 1.0 } else { 0.0 };
        }
        (self.transferred as f32 / self.size as f32).clamp(0.0, 1.0)
    }
}

/// What the queue looks like right now, for the status bar.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub active: usize,
    pub queued: usize,
    pub failed: usize,
    pub done: usize,
    pub bytes_per_second: u64,
}

/// Signals a running worker to stop between parts. Checked often enough that a
/// pause feels immediate, but never mid-request, so progress stays truthful.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    Run = 0,
    Pause = 1,
    Cancel = 2,
}

impl Control {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Control::Pause,
            2 => Control::Cancel,
            _ => Control::Run,
        }
    }
}

#[derive(Default)]
struct Rate {
    samples: Vec<(Instant, u64)>,
}

impl Rate {
    /// Bytes per second over roughly the last three seconds, so the number in
    /// the UI reacts to a stall instead of averaging it away.
    fn observe(&mut self, total: u64) -> u64 {
        let now = Instant::now();
        self.samples.push((now, total));
        self.samples
            .retain(|(at, _)| now.duration_since(*at).as_secs_f32() < 3.0);

        let (Some((first_at, first_bytes)), Some((last_at, last_bytes))) =
            (self.samples.first(), self.samples.last())
        else {
            return 0;
        };
        let seconds = last_at.duration_since(*first_at).as_secs_f32();
        if seconds < 0.2 {
            return 0;
        }
        ((last_bytes.saturating_sub(*first_bytes)) as f32 / seconds) as u64
    }
}

struct Inner {
    journal: Journal,
    jobs: Mutex<HashMap<i64, Job>>,
    controls: Mutex<HashMap<i64, Arc<AtomicU8>>>,
    job_slots: Arc<Semaphore>,
    part_concurrency: usize,
    rate: Mutex<Rate>,
}

#[derive(Clone)]
pub struct TransferEngine {
    inner: Arc<Inner>,
}

impl TransferEngine {
    /// Opens the queue at `path`, restoring anything left unfinished.
    /// Jobs that were mid-flight are marked paused rather than resumed silently:
    /// after a crash the user should decide what starts moving again.
    pub fn open(path: &Path) -> Result<Self> {
        Self::from_journal(Journal::open(path)?)
    }

    pub fn in_memory() -> Result<Self> {
        Self::from_journal(Journal::in_memory()?)
    }

    fn from_journal(journal: Journal) -> Result<Self> {
        let restored = journal.load_all()?;
        let mut jobs = HashMap::new();
        for mut job in restored {
            if job.state == JobState::Running {
                job.state = JobState::Paused;
                journal.set_state(job.id, JobState::Paused)?;
            }
            jobs.insert(job.id, job);
        }

        Ok(Self {
            inner: Arc::new(Inner {
                journal,
                jobs: Mutex::new(jobs),
                controls: Mutex::new(HashMap::new()),
                job_slots: Arc::new(Semaphore::new(DEFAULT_JOB_CONCURRENCY)),
                part_concurrency: DEFAULT_PART_CONCURRENCY,
                rate: Mutex::new(Rate::default()),
            }),
        })
    }

    /// Cheap enough to call every frame.
    pub fn snapshot(&self) -> Vec<Job> {
        let jobs = self.inner.jobs.lock().unwrap();
        let mut list: Vec<Job> = jobs.values().cloned().collect();
        list.sort_by_key(|job| job.id);
        list
    }

    pub fn stats(&self) -> Stats {
        let jobs = self.inner.jobs.lock().unwrap();
        let mut stats = Stats::default();
        let mut moved = 0;

        for job in jobs.values() {
            match job.state {
                JobState::Running => stats.active += 1,
                JobState::Queued => stats.queued += 1,
                JobState::Failed => stats.failed += 1,
                JobState::Done => stats.done += 1,
                _ => {}
            }
            if job.state != JobState::Done {
                moved += job.transferred;
            }
        }

        stats.bytes_per_second = if stats.active > 0 {
            self.inner.rate.lock().unwrap().observe(moved)
        } else {
            0
        };
        stats
    }

    pub fn has_active_work(&self) -> bool {
        self.inner
            .jobs
            .lock()
            .unwrap()
            .values()
            .any(|job| job.state.is_active())
    }

    /// Queues every file under `paths`, walking directories so a folder dropped
    /// from the file manager arrives with its structure intact.
    pub async fn enqueue_uploads(
        &self,
        client: S3Client,
        bucket: &str,
        prefix: &str,
        paths: &[PathBuf],
    ) -> Result<Vec<i64>> {
        let mut ids = Vec::new();
        for path in paths {
            for (file, key_suffix) in walk_upload_source(path)? {
                let size = tokio::fs::metadata(&file)
                    .await
                    .with_context(|| format!("reading {}", file.display()))?
                    .len();

                let id = self.insert(Job {
                    id: 0,
                    direction: Direction::Upload,
                    local: file,
                    bucket: bucket.to_string(),
                    key: format!("{prefix}{key_suffix}"),
                    size,
                    transferred: 0,
                    state: JobState::Queued,
                    error: None,
                    created_at: now_epoch(),
                })?;
                ids.push(id);
                self.start(id, client.clone());
            }
        }
        Ok(ids)
    }

    /// Queues a download of `key` into `destination_dir`.
    pub async fn enqueue_download(
        &self,
        client: S3Client,
        bucket: &str,
        key: &str,
        destination_dir: &Path,
    ) -> Result<i64> {
        let head = client.head_object(bucket, key).await?;
        let name = key.rsplit('/').next().unwrap_or(key);

        let id = self.insert(Job {
            id: 0,
            direction: Direction::Download,
            local: destination_dir.join(name),
            bucket: bucket.to_string(),
            key: key.to_string(),
            size: head.size.max(0) as u64,
            transferred: 0,
            state: JobState::Queued,
            error: None,
            created_at: now_epoch(),
        })?;
        self.inner.journal.set_etag(id, head.etag.as_deref())?;
        self.start(id, client);
        Ok(id)
    }

    fn insert(&self, mut job: Job) -> Result<i64> {
        let id = self.inner.journal.insert(&job)?;
        job.id = id;
        self.inner.jobs.lock().unwrap().insert(id, job);
        Ok(id)
    }

    /// Spawns the worker. It waits on a permit first, so queued jobs stay
    /// queued rather than all opening connections at once.
    fn start(&self, id: i64, client: S3Client) {
        let control = Arc::new(AtomicU8::new(Control::Run as u8));
        self.inner
            .controls
            .lock()
            .unwrap()
            .insert(id, control.clone());

        let engine = self.clone();
        tokio::spawn(async move {
            let Ok(_permit) = engine.inner.job_slots.clone().acquire_owned().await else {
                return;
            };
            // The user may have paused or cancelled while this waited in line.
            if Control::from_u8(control.load(Ordering::SeqCst)) != Control::Run {
                return;
            }
            worker::run(engine, id, client, control).await;
        });
    }

    pub fn pause(&self, id: i64) {
        if let Some(control) = self.inner.controls.lock().unwrap().get(&id) {
            control.store(Control::Pause as u8, Ordering::SeqCst);
        }
        // Reflect it immediately; the worker confirms when it stops.
        self.set_state_if_active(id, JobState::Paused);
    }

    pub fn cancel(&self, id: i64) {
        if let Some(control) = self.inner.controls.lock().unwrap().get(&id) {
            control.store(Control::Cancel as u8, Ordering::SeqCst);
        }
        self.set_state_if_active(id, JobState::Canceled);
    }

    /// Restarts a paused or failed job. Progress already journalled is kept, so
    /// a large upload continues from the last accepted part.
    pub fn resume(&self, id: i64, client: S3Client) {
        let should_start = {
            let mut jobs = self.inner.jobs.lock().unwrap();
            match jobs.get_mut(&id) {
                Some(job) if matches!(job.state, JobState::Paused | JobState::Failed) => {
                    job.state = JobState::Queued;
                    job.error = None;
                    true
                }
                _ => false,
            }
        };

        if should_start {
            _ = self.inner.journal.set_state(id, JobState::Queued);
            self.start(id, client);
        }
    }

    /// Drops one job from the queue. Cancels it first if it is still moving, so
    /// a removed upload does not keep billing for its parts.
    pub fn remove_job(&self, id: i64) {
        let was_active = self
            .inner
            .jobs
            .lock()
            .unwrap()
            .get(&id)
            .is_some_and(|job| job.state.is_active());
        if was_active {
            self.cancel(id);
        }
        _ = self.inner.journal.remove(id);
        self.inner.jobs.lock().unwrap().remove(&id);
        self.inner.controls.lock().unwrap().remove(&id);
    }

    pub fn clear_finished(&self) {
        _ = self.inner.journal.clear_finished();
        self.inner
            .jobs
            .lock()
            .unwrap()
            .retain(|_, job| !job.state.is_finished());
    }

    fn set_state_if_active(&self, id: i64, state: JobState) {
        let mut jobs = self.inner.jobs.lock().unwrap();
        if let Some(job) = jobs.get_mut(&id) {
            if job.state.is_active() {
                job.state = state;
                _ = self.inner.journal.set_state(id, state);
            }
        }
    }

    // Used by the worker.

    fn job(&self, id: i64) -> Option<Job> {
        self.inner.jobs.lock().unwrap().get(&id).cloned()
    }

    fn set_state(&self, id: i64, state: JobState) {
        if let Some(job) = self.inner.jobs.lock().unwrap().get_mut(&id) {
            job.state = state;
        }
        _ = self.inner.journal.set_state(id, state);
    }

    fn set_failed(&self, id: i64, error: String) {
        if let Some(job) = self.inner.jobs.lock().unwrap().get_mut(&id) {
            job.state = JobState::Failed;
            job.error = Some(error.clone());
        }
        _ = self.inner.journal.set_failed(id, &error);
    }

    fn add_progress(&self, id: i64, bytes: u64) {
        let transferred = {
            let mut jobs = self.inner.jobs.lock().unwrap();
            match jobs.get_mut(&id) {
                Some(job) => {
                    job.transferred = (job.transferred + bytes).min(job.size.max(job.transferred));
                    job.transferred
                }
                None => return,
            }
        };
        _ = self.inner.journal.set_progress(id, transferred);
    }

    fn set_progress(&self, id: i64, transferred: u64) {
        if let Some(job) = self.inner.jobs.lock().unwrap().get_mut(&id) {
            job.transferred = transferred;
        }
        _ = self.inner.journal.set_progress(id, transferred);
    }

    fn journal(&self) -> &Journal {
        &self.inner.journal
    }

    fn part_concurrency(&self) -> usize {
        self.inner.part_concurrency
    }
}

/// Part size that keeps the object under the 10,000-part ceiling while staying
/// at or above S3's 5 MiB minimum.
pub fn part_size_for(total: u64) -> u64 {
    let mut size = DEFAULT_PART_SIZE;
    while total.div_ceil(size) > MAX_PARTS {
        size *= 2;
    }
    size.max(MIN_PART_SIZE)
}

/// Expands one dropped path into the files to upload, each with the key suffix
/// it should get. A dropped directory keeps its own name as the top folder.
fn walk_upload_source(path: &Path) -> Result<Vec<(PathBuf, String)>> {
    let metadata = std::fs::metadata(path)
        .with_context(|| format!("reading {}", path.display()))?;

    if metadata.is_file() {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        return Ok(vec![(path.to_path_buf(), name)]);
    }

    if !metadata.is_dir() {
        // Sockets, fifos and the like are not something we can upload.
        return Ok(Vec::new());
    }

    let root_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "folder".into());

    let mut files = Vec::new();
    let mut stack = vec![(path.to_path_buf(), root_name)];

    while let Some((directory, relative)) = stack.pop() {
        for entry in std::fs::read_dir(&directory)
            .with_context(|| format!("reading {}", directory.display()))?
        {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let child_relative = format!("{relative}/{name}");
            let file_type = entry.file_type()?;

            if file_type.is_dir() {
                stack.push((entry.path(), child_relative));
            } else if file_type.is_file() {
                files.push((entry.path(), child_relative));
            }
            // Symlinks are skipped: following them can escape the dropped tree.
        }
    }

    files.sort_by(|a, b| a.1.cmp(&b.1));
    Ok(files)
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_size_grows_to_stay_under_the_part_ceiling() {
        assert_eq!(part_size_for(100 * 1024 * 1024), DEFAULT_PART_SIZE);

        // 10 TiB at 16 MiB parts would need 655,360 parts, far past the limit.
        let huge = 10 * 1024 * 1024 * 1024 * 1024_u64;
        let size = part_size_for(huge);
        assert!(huge.div_ceil(size) <= MAX_PARTS, "{} parts", huge.div_ceil(size));

        // Never below what S3 accepts.
        assert!(part_size_for(1) >= MIN_PART_SIZE);
    }

    #[test]
    fn job_fraction_is_clamped_and_handles_empty_objects() {
        let mut job = Job {
            id: 1,
            direction: Direction::Upload,
            local: PathBuf::from("/tmp/a"),
            bucket: "b".into(),
            key: "k".into(),
            size: 100,
            transferred: 50,
            state: JobState::Running,
            error: None,
            created_at: 0,
        };
        assert_eq!(job.fraction(), 0.5);

        job.transferred = 500;
        assert_eq!(job.fraction(), 1.0, "progress must never exceed the bar");

        job.size = 0;
        job.state = JobState::Running;
        assert_eq!(job.fraction(), 0.0);
        job.state = JobState::Done;
        assert_eq!(job.fraction(), 1.0, "a finished empty object reads as done");
    }

    #[test]
    fn display_name_uses_the_file_for_uploads_and_the_key_for_downloads() {
        let mut job = Job {
            id: 1,
            direction: Direction::Upload,
            local: PathBuf::from("/home/me/photos/beach.jpg"),
            bucket: "b".into(),
            key: "trip/2026/beach.jpg".into(),
            size: 1,
            transferred: 0,
            state: JobState::Queued,
            error: None,
            created_at: 0,
        };
        assert_eq!(job.display_name(), "beach.jpg");

        job.direction = Direction::Download;
        assert_eq!(job.display_name(), "beach.jpg");
    }

    #[test]
    fn walking_a_dropped_directory_keeps_its_structure() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("trip");
        std::fs::create_dir_all(base.join("day-1")).unwrap();
        std::fs::write(base.join("readme.txt"), b"hi").unwrap();
        std::fs::write(base.join("day-1").join("a.jpg"), b"x").unwrap();

        let mut found = walk_upload_source(&base).unwrap();
        found.sort_by(|a, b| a.1.cmp(&b.1));

        let suffixes: Vec<_> = found.iter().map(|(_, suffix)| suffix.as_str()).collect();
        assert_eq!(suffixes, vec!["trip/day-1/a.jpg", "trip/readme.txt"]);
    }

    #[test]
    fn walking_a_single_file_yields_just_its_name() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("notes.md");
        std::fs::write(&file, b"hi").unwrap();

        let found = walk_upload_source(&file).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, "notes.md");
    }

    #[test]
    fn restoring_marks_interrupted_jobs_paused_rather_than_running() {
        let journal = Journal::in_memory().unwrap();
        let id = journal
            .insert(&Job {
                id: 0,
                direction: Direction::Upload,
                local: PathBuf::from("/tmp/a"),
                bucket: "b".into(),
                key: "k".into(),
                size: 10,
                transferred: 5,
                state: JobState::Running,
                error: None,
                created_at: 0,
            })
            .unwrap();
        journal.set_state(id, JobState::Running).unwrap();

        let engine = TransferEngine::from_journal(journal).unwrap();
        let jobs = engine.snapshot();
        assert_eq!(jobs.len(), 1);
        assert_eq!(
            jobs[0].state,
            JobState::Paused,
            "a crash must not silently restart transfers"
        );
        assert_eq!(jobs[0].transferred, 5, "progress is kept for the resume");
    }

    #[test]
    fn stats_count_each_state() {
        let engine = TransferEngine::in_memory().unwrap();
        let make = |state: JobState| Job {
            id: 0,
            direction: Direction::Upload,
            local: PathBuf::from("/tmp/a"),
            bucket: "b".into(),
            key: "k".into(),
            size: 10,
            transferred: 0,
            state,
            error: None,
            created_at: 0,
        };

        for state in [
            JobState::Queued,
            JobState::Queued,
            JobState::Failed,
            JobState::Done,
        ] {
            let id = engine.insert(make(state)).unwrap();
            engine.set_state(id, state);
        }

        let stats = engine.stats();
        assert_eq!(stats.queued, 2);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.done, 1);
        assert_eq!(stats.bytes_per_second, 0, "nothing is running");
        assert!(engine.has_active_work());
    }

    #[test]
    fn removing_a_job_takes_it_out_of_the_queue_and_the_journal() {
        let engine = TransferEngine::in_memory().unwrap();
        let id = engine
            .insert(Job {
                id: 0,
                direction: Direction::Upload,
                local: PathBuf::from("/tmp/a"),
                bucket: "b".into(),
                key: "k".into(),
                size: 10,
                transferred: 0,
                state: JobState::Queued,
                error: None,
                created_at: 0,
            })
            .unwrap();

        engine.remove_job(id);
        assert!(engine.snapshot().is_empty());
        assert!(
            engine.journal().load_all().unwrap().is_empty(),
            "a removed job must not come back on restart"
        );
    }

    #[test]
    fn clearing_finished_leaves_failed_jobs_for_retry() {
        let engine = TransferEngine::in_memory().unwrap();
        let make = |state: JobState| Job {
            id: 0,
            direction: Direction::Upload,
            local: PathBuf::from("/tmp/a"),
            bucket: "b".into(),
            key: "k".into(),
            size: 10,
            transferred: 0,
            state,
            error: None,
            created_at: 0,
        };
        for state in [JobState::Done, JobState::Failed, JobState::Canceled] {
            let id = engine.insert(make(state)).unwrap();
            engine.set_state(id, state);
        }

        engine.clear_finished();
        let left = engine.snapshot();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].state, JobState::Failed);
    }
}
