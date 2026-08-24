//! The transfer loops.
//!
//! Both directions follow the same shape: decide whether the object is small
//! enough for one request, otherwise split it into parts, skip whatever is
//! already recorded as done, and check the control flag between parts so a
//! pause or cancel lands quickly without ever abandoning a request mid-flight.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use s3core::{CompletedPart, S3Client};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::checksum;
use crate::{
    part_size_for, Control, Direction, Job, JobState, TransferEngine, MULTIPART_THRESHOLD,
};

/// Why a worker stopped.
enum Outcome {
    Done,
    Paused,
    Canceled,
}

pub(crate) async fn run(engine: TransferEngine, id: i64, client: S3Client, control: Arc<AtomicU8>) {
    let Some(job) = engine.job(id) else { return };
    engine.set_state(id, JobState::Running);

    let result = match job.direction {
        Direction::Upload => upload(&engine, &job, &client, &control).await,
        Direction::Download => download(&engine, &job, &client, &control).await,
    };

    match result {
        Ok(Outcome::Done) => {
            engine.set_progress(id, job.size);
            engine.set_state(id, JobState::Done);
        }
        Ok(Outcome::Paused) => engine.set_state(id, JobState::Paused),
        Ok(Outcome::Canceled) => engine.set_state(id, JobState::Canceled),
        // `{:#}` includes the anyhow context chain, which is where the useful
        // detail lives (which key, which part).
        Err(error) => engine.set_failed(id, format!("{error:#}")),
    }
}

fn control_of(control: &AtomicU8) -> Control {
    Control::from_u8(control.load(Ordering::SeqCst))
}

// ----------------------------------------------------------------- uploads

async fn upload(
    engine: &TransferEngine,
    job: &Job,
    client: &S3Client,
    control: &AtomicU8,
) -> Result<Outcome> {
    if job.size < MULTIPART_THRESHOLD {
        let body = tokio::fs::read(&job.local)
            .await
            .with_context(|| format!("reading {}", job.local.display()))?;
        client.put_object(&job.bucket, &job.key, body).await?;
        return Ok(Outcome::Done);
    }

    // Reuse the upload id from a previous attempt so the server's parts count.
    let upload_id = match engine.journal().upload_id(job.id)? {
        Some(existing) => existing,
        None => {
            let created = client
                .create_multipart_upload(&job.bucket, &job.key)
                .await?;
            engine.journal().set_upload_id(job.id, &created)?;
            created
        }
    };

    // The server is the authority on what it holds: a part we journalled but
    // that never landed would otherwise be skipped forever.
    let server_parts = client
        .list_parts(&job.bucket, &job.key, &upload_id)
        .await
        .unwrap_or_default();
    engine.journal().clear_parts(job.id)?;
    for part in &server_parts {
        engine.journal().record_part(job.id, part)?;
    }

    let mut done: BTreeMap<i32, CompletedPart> = server_parts
        .into_iter()
        .map(|part| (part.part_number, part))
        .collect();
    engine.set_progress(job.id, done.values().map(|part| part.size).sum());

    let part_size = part_size_for(job.size);
    let part_count = job.size.div_ceil(part_size) as i32;

    let slots = Arc::new(Semaphore::new(engine.part_concurrency()));
    let mut in_flight = JoinSet::new();
    let mut stopped_early = None;

    for number in 1..=part_count {
        if done.contains_key(&number) {
            continue;
        }
        match control_of(control) {
            Control::Run => {}
            other => {
                stopped_early = Some(other);
                break;
            }
        }

        let permit = slots.clone().acquire_owned().await?;
        let client = client.clone();
        let path = job.local.clone();
        let bucket = job.bucket.clone();
        let key = job.key.clone();
        let upload_id = upload_id.clone();
        let offset = (number as u64 - 1) * part_size;
        let length = part_size.min(job.size - offset);

        // Bandwidth cap is charged before the bytes move, so a queue-wide limit
        // holds no matter how many parts are in flight.
        engine.throttle().acquire(length).await;

        in_flight.spawn(async move {
            let _permit = permit;
            let body = read_chunk(&path, offset, length).await?;
            let uploaded = client
                .upload_part(&bucket, &key, &upload_id, number, body)
                .await?;
            anyhow::Ok(CompletedPart {
                part_number: number,
                etag: uploaded.etag,
                size: length,
                checksum_crc32: uploaded.checksum_crc32,
            })
        });
    }

    // Drain whatever is already in flight, recording each acceptance as it lands
    // so a crash right now still resumes from the right place.
    let mut failure = None;
    while let Some(joined) = in_flight.join_next().await {
        match joined {
            Ok(Ok(part)) => {
                engine.journal().record_part(job.id, &part)?;
                engine.add_progress(job.id, part.size);
                done.insert(part.part_number, part);
            }
            Ok(Err(error)) => failure = Some(error),
            Err(error) => failure = Some(anyhow::anyhow!("part task panicked: {error}")),
        }
    }

    if let Some(error) = failure {
        return Err(error);
    }

    match stopped_early {
        Some(Control::Cancel) => {
            // Release the storage S3 is already billing for.
            client
                .abort_multipart_upload(&job.bucket, &job.key, &upload_id)
                .await
                .ok();
            engine.journal().clear_parts(job.id)?;
            return Ok(Outcome::Canceled);
        }
        Some(Control::Pause) => return Ok(Outcome::Paused),
        _ => {}
    }

    if done.len() as i32 != part_count {
        anyhow::bail!(
            "only {} of {part_count} parts completed for s3://{}/{}",
            done.len(),
            job.bucket,
            job.key
        );
    }

    client
        .complete_multipart_upload(
            &job.bucket,
            &job.key,
            &upload_id,
            done.into_values().collect(),
        )
        .await?;
    Ok(Outcome::Done)
}

/// Reads `length` bytes at `offset`. Each part opens the file itself, which
/// keeps the concurrent reads independent and works the same on every platform.
async fn read_chunk(path: &Path, offset: u64, length: u64) -> Result<Vec<u8>> {
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;

    let mut buffer = vec![0u8; length as usize];
    file.read_exact(&mut buffer)
        .await
        .with_context(|| format!("reading {length} bytes at {offset} of {}", path.display()))?;
    Ok(buffer)
}

// --------------------------------------------------------------- downloads

async fn download(
    engine: &TransferEngine,
    job: &Job,
    client: &S3Client,
    control: &AtomicU8,
) -> Result<Outcome> {
    if let Some(parent) = job.local.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    // `If-Match` on every read: if the object is replaced while we are pulling
    // it, the request fails instead of stitching two versions into one file.
    let etag = engine.journal().etag(job.id)?;
    let temp = temp_path(&job.local);

    if job.size < MULTIPART_THRESHOLD {
        let bytes = client
            .get_range(&job.bucket, &job.key, 0..job.size.max(1), etag.as_deref())
            .await?;
        tokio::fs::write(&temp, &bytes)
            .await
            .with_context(|| format!("writing {}", temp.display()))?;
        tokio::fs::rename(&temp, &job.local)
            .await
            .with_context(|| format!("renaming into {}", job.local.display()))?;
        verify_download(client, job).await?;
        return Ok(Outcome::Done);
    }

    let part_size = part_size_for(job.size);
    let part_count = job.size.div_ceil(part_size) as i32;

    // Pre-size the file so chunks can be written at their final offsets in any
    // order; a resumed download reuses the same partial file.
    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&temp)
        .await
        .with_context(|| format!("opening {}", temp.display()))?;
    file.set_len(job.size).await?;
    drop(file);

    let mut done: BTreeMap<i32, CompletedPart> = engine
        .journal()
        .parts(job.id)?
        .into_iter()
        .map(|part| (part.part_number, part))
        .collect();
    engine.set_progress(job.id, done.values().map(|part| part.size).sum());

    let slots = Arc::new(Semaphore::new(engine.part_concurrency()));
    let mut in_flight = JoinSet::new();
    let mut stopped_early = None;

    for number in 1..=part_count {
        if done.contains_key(&number) {
            continue;
        }
        match control_of(control) {
            Control::Run => {}
            other => {
                stopped_early = Some(other);
                break;
            }
        }

        let permit = slots.clone().acquire_owned().await?;
        let client = client.clone();
        let bucket = job.bucket.clone();
        let key = job.key.clone();
        let etag = etag.clone();
        let temp = temp.clone();
        let offset = (number as u64 - 1) * part_size;
        let length = part_size.min(job.size - offset);

        engine.throttle().acquire(length).await;

        in_flight.spawn(async move {
            let _permit = permit;
            let bytes = client
                .get_range(&bucket, &key, offset..offset + length, etag.as_deref())
                .await?;
            write_chunk(&temp, offset, &bytes).await?;
            anyhow::Ok(CompletedPart {
                part_number: number,
                // Downloads have no per-part ETag; the column carries the range
                // so a half-finished file is inspectable.
                etag: format!("bytes={offset}-{}", offset + length - 1),
                size: bytes.len() as u64,
                // A download part is never sent back to S3, so it has no
                // checksum to repeat.
                checksum_crc32: None,
            })
        });
    }

    let mut failure = None;
    while let Some(joined) = in_flight.join_next().await {
        match joined {
            Ok(Ok(part)) => {
                engine.journal().record_part(job.id, &part)?;
                engine.add_progress(job.id, part.size);
                done.insert(part.part_number, part);
            }
            Ok(Err(error)) => failure = Some(error),
            Err(error) => failure = Some(anyhow::anyhow!("part task panicked: {error}")),
        }
    }

    if let Some(error) = failure {
        return Err(error);
    }

    match stopped_early {
        Some(Control::Cancel) => {
            tokio::fs::remove_file(&temp).await.ok();
            engine.journal().clear_parts(job.id)?;
            return Ok(Outcome::Canceled);
        }
        Some(Control::Pause) => return Ok(Outcome::Paused),
        _ => {}
    }

    if done.len() as i32 != part_count {
        anyhow::bail!(
            "only {} of {part_count} ranges completed for s3://{}/{}",
            done.len(),
            job.bucket,
            job.key
        );
    }

    tokio::fs::rename(&temp, &job.local)
        .await
        .with_context(|| format!("renaming into {}", job.local.display()))?;
    verify_download(client, job).await?;
    engine.journal().clear_parts(job.id)?;
    Ok(Outcome::Done)
}

async fn write_chunk(path: &Path, offset: u64, bytes: &[u8]) -> Result<()> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .await
        .with_context(|| format!("opening {}", path.display()))?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    file.write_all(bytes)
        .await
        .with_context(|| format!("writing {} bytes at {offset}", bytes.len()))?;
    file.flush().await?;
    Ok(())
}

/// Checks the downloaded bytes against the server's checksum.
///
/// Runs after the rename because a file that fails here is still the file the
/// user asked for — leaving it in place with a loud error is more useful than
/// deleting it silently and making them download it again to see the same
/// failure.
///
/// The hashing is blocking work over the whole file, so it goes to a blocking
/// thread rather than stalling a runtime worker for the length of a large read.
async fn verify_download(client: &S3Client, job: &Job) -> Result<()> {
    let head = client.head_object(&job.bucket, &job.key).await?;
    let Some(expected) = head.checksum_crc32 else {
        // No checksum from the server: nothing to compare, and that is normal
        // on older objects and on providers that never implemented them.
        return Ok(());
    };

    let path = job.local.clone();
    let verification =
        tokio::task::spawn_blocking(move || checksum::verify(&path, Some(&expected)))
            .await
            .context("checksum verification task did not complete")??;

    if verification == checksum::Verification::Mismatch {
        // Marker text matched by `app::failure::classify` — not an SDK
        // error, so there is no error code to key a translated summary off
        // of otherwise.
        anyhow::bail!(
            "checksum mismatch for {}: downloaded file differs from the server's version",
            job.key
        );
    }
    Ok(())
}

/// Downloads land in a sibling `.s3part` file and are renamed on completion, so
/// an interrupted transfer never looks like a finished file.
fn temp_path(final_path: &Path) -> PathBuf {
    let mut name = final_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".into());
    name.push_str(".s3part");
    final_path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_path_keeps_the_original_extension_visible() {
        assert_eq!(
            temp_path(Path::new("/tmp/report.tar.gz")),
            PathBuf::from("/tmp/report.tar.gz.s3part"),
            "appending, not replacing, keeps the real name recognizable"
        );
    }

    #[tokio::test]
    async fn read_chunk_reads_exactly_the_requested_window() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        tokio::fs::write(&path, b"0123456789").await.unwrap();

        assert_eq!(read_chunk(&path, 0, 4).await.unwrap(), b"0123");
        assert_eq!(read_chunk(&path, 6, 4).await.unwrap(), b"6789");

        // Past the end must fail loudly rather than return a short buffer that
        // would silently corrupt a part.
        assert!(read_chunk(&path, 8, 4).await.is_err());
    }

    #[tokio::test]
    async fn chunks_written_out_of_order_assemble_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.bin");

        let file = tokio::fs::File::create(&path).await.unwrap();
        file.set_len(10).await.unwrap();
        drop(file);

        // Later range first, exactly as concurrent parts may complete.
        write_chunk(&path, 5, b"56789").await.unwrap();
        write_chunk(&path, 0, b"01234").await.unwrap();

        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"0123456789");
    }
}
