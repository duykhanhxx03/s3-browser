//! Transfer engine against a local MinIO.
//!
//! Start one with `scripts/minio-dev.sh start`. Tests skip themselves when
//! nothing is listening, so `cargo test` stays green without Docker.

use std::path::Path;
use std::time::{Duration, Instant};

use s3core::{Profile, S3Client};
use transfer::{Direction, JobState, TransferEngine, MULTIPART_THRESHOLD};

const BUCKET: &str = "transfer-tests";

async fn client_or_skip() -> Option<S3Client> {
    let client = S3Client::connect(&Profile::minio_local()).await.ok()?;
    if client.list_buckets().await.is_err() {
        eprintln!("skipping: MinIO not reachable on 127.0.0.1:9000");
        return None;
    }
    // These tests bring their own bucket, so an empty MinIO is fine.
    // Ignore "already owned by you".
    client.create_bucket(BUCKET).await.ok();
    Some(client)
}

/// Polls until the job reaches a state the test cares about.
async fn wait_for(
    engine: &TransferEngine,
    id: i64,
    accept: impl Fn(JobState) -> bool,
    timeout: Duration,
) -> JobState {
    let deadline = Instant::now() + timeout;
    loop {
        let state = engine
            .snapshot()
            .into_iter()
            .find(|job| job.id == id)
            .map(|job| job.state)
            .unwrap_or(JobState::Canceled);

        if accept(state) {
            return state;
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for job {id}; last state {state:?}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn write_file(path: &Path, size: usize) -> Vec<u8> {
    // Varied bytes, so a misplaced range shows up as a content mismatch rather
    // than passing against a buffer of zeros.
    let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
    std::fs::write(path, &content).unwrap();
    content
}

#[tokio::test]
async fn uploads_a_small_file_in_one_request() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("small.bin");
    let content = write_file(&file, 4096);

    let engine = TransferEngine::in_memory().unwrap();
    let ids = engine
        .enqueue_uploads(client.clone(), BUCKET, "small/", &[file])
        .await
        .unwrap();
    assert_eq!(ids.len(), 1);

    let state = wait_for(
        &engine,
        ids[0],
        |state| matches!(state, JobState::Done | JobState::Failed),
        Duration::from_secs(30),
    )
    .await;
    let job = engine.snapshot().into_iter().next().unwrap();
    assert_eq!(state, JobState::Done, "error: {:?}", job.error);
    assert_eq!(job.transferred, job.size);
    assert_eq!(job.fraction(), 1.0);

    let head = client.head_object(BUCKET, "small/small.bin").await.unwrap();
    assert_eq!(head.size as usize, content.len());
}

#[tokio::test]
async fn uploads_a_large_file_in_parts_and_downloads_it_back_identically() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("large.bin");
    // Comfortably over the threshold, so this is a real multipart upload.
    let size = (MULTIPART_THRESHOLD as usize) + 7 * 1024 * 1024;
    let content = write_file(&file, size);

    let engine = TransferEngine::in_memory().unwrap();
    let ids = engine
        .enqueue_uploads(client.clone(), BUCKET, "large/", &[file])
        .await
        .unwrap();

    let state = wait_for(
        &engine,
        ids[0],
        |state| matches!(state, JobState::Done | JobState::Failed),
        Duration::from_secs(120),
    )
    .await;
    let job = engine.snapshot().into_iter().next().unwrap();
    assert_eq!(state, JobState::Done, "error: {:?}", job.error);

    let head = client.head_object(BUCKET, "large/large.bin").await.unwrap();
    assert_eq!(head.size as usize, size);
    assert!(
        head.etag.as_deref().is_some_and(|etag| etag.contains('-')),
        "a multipart object's ETag carries a part count, got {:?}",
        head.etag
    );

    // Completing the upload must have left no dangling multipart behind.
    let orphans = client.list_orphaned_uploads(BUCKET).await.unwrap();
    assert!(
        !orphans.iter().any(|o| o.key == "large/large.bin"),
        "completed upload left an orphan: {orphans:?}"
    );

    // Now pull it back through the ranged download path and compare bytes.
    let download_dir = tempfile::tempdir().unwrap();
    let id = engine
        .enqueue_download(client.clone(), BUCKET, "large/large.bin", download_dir.path())
        .await
        .unwrap();

    let state = wait_for(
        &engine,
        id,
        |state| matches!(state, JobState::Done | JobState::Failed),
        Duration::from_secs(120),
    )
    .await;
    let job = engine
        .snapshot()
        .into_iter()
        .find(|job| job.id == id)
        .unwrap();
    assert_eq!(state, JobState::Done, "error: {:?}", job.error);
    assert_eq!(job.direction, Direction::Download);

    let landed = download_dir.path().join("large.bin");
    assert_eq!(
        std::fs::read(&landed).unwrap(),
        content,
        "downloaded bytes differ from what was uploaded"
    );
    assert!(
        !download_dir.path().join("large.bin.s3part").exists(),
        "the partial file should be renamed away on completion"
    );
}

#[tokio::test]
async fn uploads_a_dropped_directory_keeping_its_structure() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let tree = dir.path().join("trip");
    std::fs::create_dir_all(tree.join("day-1")).unwrap();
    write_file(&tree.join("readme.txt"), 128);
    write_file(&tree.join("day-1").join("photo.bin"), 2048);

    let engine = TransferEngine::in_memory().unwrap();
    let ids = engine
        .enqueue_uploads(client.clone(), BUCKET, "dropped/", &[tree])
        .await
        .unwrap();
    assert_eq!(ids.len(), 2, "both files should be queued");

    for id in ids {
        let state = wait_for(
            &engine,
            id,
            |state| matches!(state, JobState::Done | JobState::Failed),
            Duration::from_secs(60),
        )
        .await;
        assert_eq!(state, JobState::Done);
    }

    // The dropped folder's own name is preserved as the top level.
    assert!(client
        .head_object(BUCKET, "dropped/trip/readme.txt")
        .await
        .is_ok());
    assert!(client
        .head_object(BUCKET, "dropped/trip/day-1/photo.bin")
        .await
        .is_ok());
}

#[tokio::test]
async fn cancelling_a_queued_upload_leaves_nothing_behind() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("cancelled.bin");
    write_file(&file, (MULTIPART_THRESHOLD as usize) + 1024);

    let engine = TransferEngine::in_memory().unwrap();
    let ids = engine
        .enqueue_uploads(client.clone(), BUCKET, "cancelled/", &[file])
        .await
        .unwrap();
    engine.cancel(ids[0]);

    let state = wait_for(
        &engine,
        ids[0],
        |state| !state.is_active(),
        Duration::from_secs(60),
    )
    .await;
    assert!(
        matches!(state, JobState::Canceled | JobState::Done),
        "got {state:?}"
    );

    if state == JobState::Canceled {
        // A cancelled multipart must be aborted, or S3 bills for its parts.
        let orphans = client.list_orphaned_uploads(BUCKET).await.unwrap();
        assert!(
            !orphans.iter().any(|o| o.key == "cancelled/cancelled.bin"),
            "cancel left an orphaned upload: {orphans:?}"
        );
    }
}

/// The mechanism a resumed upload depends on: parts already accepted by the
/// server are visible through ListParts, and completing with the full set works
/// even though the parts were sent by two separate attempts.
#[tokio::test]
async fn a_partial_multipart_upload_can_be_completed_later() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let key = "resume/split.bin";
    let part_size = 5 * 1024 * 1024; // S3's minimum part size
    let first: Vec<u8> = (0..part_size).map(|i| (i % 251) as u8).collect();
    let second: Vec<u8> = (0..part_size).map(|i| ((i + 7) % 251) as u8).collect();

    let upload_id = client.create_multipart_upload(BUCKET, key).await.unwrap();

    // First attempt sends one part, then "crashes".
    let etag1 = client
        .upload_part(BUCKET, key, &upload_id, 1, first.clone())
        .await
        .unwrap();

    let listed = client.list_parts(BUCKET, key, &upload_id).await.unwrap();
    assert_eq!(listed.len(), 1, "the server should report the accepted part");
    assert_eq!(listed[0].part_number, 1);
    assert_eq!(listed[0].size, part_size as u64);

    // Second attempt reconciles and sends only what is missing.
    let etag2 = client
        .upload_part(BUCKET, key, &upload_id, 2, second.clone())
        .await
        .unwrap();

    client
        .complete_multipart_upload(
            BUCKET,
            key,
            &upload_id,
            vec![
                s3core::CompletedPart {
                    part_number: 2,
                    etag: etag2.etag.clone(),
                    size: part_size as u64,
                    checksum_crc32: etag2.checksum_crc32.clone(),
                },
                // Deliberately out of order: complete_multipart_upload must sort,
                // because S3 rejects an unordered part list.
                s3core::CompletedPart {
                    part_number: 1,
                    etag: etag1.etag.clone(),
                    size: part_size as u64,
                    checksum_crc32: etag1.checksum_crc32.clone(),
                },
            ],
        )
        .await
        .unwrap();

    let head = client.head_object(BUCKET, key).await.unwrap();
    assert_eq!(head.size as usize, part_size * 2);
}

/// Abandoned uploads are invisible in a normal listing but keep costing money,
/// so the app has to be able to find and abort them.
#[tokio::test]
async fn orphaned_uploads_are_discoverable_and_abortable() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let key = "orphan/left-behind.bin";

    let upload_id = client.create_multipart_upload(BUCKET, key).await.unwrap();
    client
        .upload_part(BUCKET, key, &upload_id, 1, vec![7u8; 5 * 1024 * 1024])
        .await
        .unwrap();

    let orphans = client.list_orphaned_uploads(BUCKET).await.unwrap();
    let found = orphans
        .iter()
        .find(|o| o.key == key && o.upload_id == upload_id)
        .expect("the abandoned upload should be listed");
    assert!(found.initiated_epoch.is_some(), "age drives the cleanup UI");

    // It is not visible as an object.
    assert!(client.head_object(BUCKET, key).await.is_err());

    client
        .abort_multipart_upload(BUCKET, key, &upload_id)
        .await
        .unwrap();

    let after = client.list_orphaned_uploads(BUCKET).await.unwrap();
    assert!(
        !after.iter().any(|o| o.upload_id == upload_id),
        "abort should have removed it"
    );
}

#[tokio::test]
async fn download_verifies_the_server_checksum() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("verified.bin");
    let content = write_file(&file, 64 * 1024);

    let engine = TransferEngine::in_memory().unwrap();
    let ids = engine
        .enqueue_uploads(client.clone(), BUCKET, "checksum/", &[file])
        .await
        .unwrap();
    wait_for(
        &engine,
        ids[0],
        |state| matches!(state, JobState::Done | JobState::Failed),
        Duration::from_secs(60),
    )
    .await;

    // Whether this server reports a checksum at all decides what the test can
    // prove; say which case ran rather than passing silently either way.
    let head = client
        .head_object(BUCKET, "checksum/verified.bin")
        .await
        .unwrap();
    match &head.checksum_crc32 {
        Some(reported) => {
            // The server's value must match one computed locally, or the
            // download check would reject every correct file.
            std::fs::write(dir.path().join("copy.bin"), &content).unwrap();
            let local = transfer::checksum::crc32_of_file(&dir.path().join("copy.bin")).unwrap();
            assert_eq!(&local, reported, "local CRC32 disagrees with the server");
        }
        None => eprintln!("server không trả x-amz-checksum-crc32; chỉ kiểm được đường bỏ qua"),
    }

    // Either way the download must succeed: a reported checksum should match,
    // and an absent one must not be treated as a failure.
    let landing = tempfile::tempdir().unwrap();
    let id = engine
        .enqueue_download(
            client.clone(),
            BUCKET,
            "checksum/verified.bin",
            landing.path(),
        )
        .await
        .unwrap();
    let state = wait_for(
        &engine,
        id,
        |state| matches!(state, JobState::Done | JobState::Failed),
        Duration::from_secs(60),
    )
    .await;
    let job = engine
        .snapshot()
        .into_iter()
        .find(|job| job.id == id)
        .unwrap();
    assert_eq!(state, JobState::Done, "error: {:?}", job.error);
    assert_eq!(
        std::fs::read(landing.path().join("verified.bin")).unwrap(),
        content
    );

    client
        .delete_object(BUCKET, "checksum/verified.bin")
        .await
        .unwrap();
}
