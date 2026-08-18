//! Integration tests against a local MinIO.
//!
//! Start one with:
//!   docker run -d --name s3browser-minio -p 9000:9000 \
//!     -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
//!     minio/minio server /data
//!
//! Tests skip themselves when nothing is listening, so `cargo test` stays green
//! on a machine without Docker.

use s3core::{Profile, S3Client};

/// Returns a client only when MinIO is reachable *and* seeded. Three outcomes
/// have to stay distinguishable: no server (skip), server without the fixture
/// data (skip, with the command to fix it), and a real assertion failure.
async fn client_or_skip() -> Option<S3Client> {
    let client = match S3Client::connect(&Profile::minio_local()).await {
        Ok(client) => client,
        Err(error) => {
            eprintln!("skipping: cannot build client: {error}");
            return None;
        }
    };

    let buckets = match client.list_buckets().await {
        Ok(buckets) => buckets,
        Err(_) => {
            eprintln!("skipping: MinIO not reachable on 127.0.0.1:9000");
            return None;
        }
    };

    if !buckets.iter().any(|bucket| bucket == "demo-bucket") {
        eprintln!(
            "skipping: MinIO has no fixture data; run scripts/minio-dev.sh reset --large"
        );
        return None;
    }
    Some(client)
}

#[tokio::test]
async fn lists_buckets() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let buckets = client.list_buckets().await.expect("ListBuckets");
    assert!(
        buckets.iter().any(|b| b == "demo-bucket"),
        "expected demo-bucket in {buckets:?}"
    );
    assert!(
        buckets.iter().any(|b| b == "photos-2026"),
        "the fixture seeds a second bucket too: {buckets:?}"
    );
}

#[tokio::test]
async fn lists_root_as_folders_and_files() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let page = client
        .list_page("demo-bucket", "", None)
        .await
        .expect("ListObjectsV2");

    let folders: Vec<_> = page
        .entries
        .iter()
        .filter(|e| e.is_folder)
        .map(|e| e.name.as_str())
        .collect();
    assert!(
        folders.contains(&"reports") && folders.contains(&"logs"),
        "expected prefix folders, got {folders:?}"
    );

    // The delimiter keeps nested keys out of the root listing.
    let files: Vec<_> = page
        .entries
        .iter()
        .filter(|e| !e.is_folder)
        .map(|e| e.name.as_str())
        .collect();
    assert!(files.contains(&"readme.txt"), "got {files:?}");
    assert!(
        !files.iter().any(|name| name.contains('/')),
        "nested keys leaked into the root listing: {files:?}"
    );

    let blob = page
        .entries
        .iter()
        .find(|e| e.name == "blob.bin")
        .expect("blob.bin");
    assert_eq!(blob.size, 3_000_000);
}

/// Exercises the write path end to end on a scratch bucket: create, add a
/// folder, delete the folder recursively, then remove the bucket.
#[tokio::test]
async fn creates_and_deletes_buckets_and_folders() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let bucket = format!("scratch-{}", std::process::id());

    client.create_bucket(&bucket).await.expect("CreateBucket");
    assert!(client.list_buckets().await.unwrap().contains(&bucket));

    let key = client
        .create_folder(&bucket, "", "uploads")
        .await
        .expect("create_folder");
    assert_eq!(key, "uploads/", "folders are zero-byte `prefix/` objects");

    // Nested content, so the delete has to expand the prefix rather than just
    // removing the placeholder.
    client
        .create_folder(&bucket, "uploads/", "2026")
        .await
        .expect("nested folder");

    let page = client.list_page(&bucket, "", None).await.expect("list");
    let folders: Vec<_> = page.entries.iter().filter(|e| e.is_folder).collect();
    assert_eq!(folders.len(), 1);
    assert_eq!(folders[0].name, "uploads");

    let report = client
        .delete_entries(&bucket, &page.entries)
        .await
        .expect("delete_entries");
    assert!(report.errors.is_empty(), "errors: {:?}", report.errors);
    assert!(report.deleted >= 2, "deleted {} keys", report.deleted);

    let after = client.list_page(&bucket, "", None).await.expect("list");
    assert!(after.entries.is_empty(), "left over: {:?}", after.entries);

    client.delete_bucket(&bucket).await.expect("DeleteBucket");
    assert!(!client.list_buckets().await.unwrap().contains(&bucket));
}

#[tokio::test]
async fn lists_inside_a_prefix() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let page = client
        .list_page("demo-bucket", "reports/", None)
        .await
        .expect("ListObjectsV2");

    assert_eq!(page.entries.len(), 5, "got {:?}", page.entries);
    assert!(page.entries.iter().all(|e| !e.is_folder));
    assert!(page.entries.iter().all(|e| e.key.starts_with("reports/")));
    // Names are shown relative to the prefix, not as full keys.
    assert!(page.entries.iter().any(|e| e.name == "file-1.txt"));
}

/// The 1000-key page cap is the reason the UI pages at all; this proves the
/// continuation token actually walks a prefix larger than one page.
#[tokio::test]
async fn pages_through_a_prefix_larger_than_one_page() {
    let Some(client) = client_or_skip().await else {
        return;
    };

    let first = client
        .list_page("demo-bucket", "many/", None)
        .await
        .expect("first page");
    if first.entries.is_empty() {
        eprintln!("skipping: run scripts/minio-dev.sh reset --large to seed many/");
        return;
    }

    assert_eq!(first.entries.len(), 1000, "S3 caps a page at 1000 keys");
    let token = first
        .continuation
        .clone()
        .expect("a truncated listing must carry a continuation token");

    let second = client
        .list_page("demo-bucket", "many/", Some(token))
        .await
        .expect("second page");
    assert_eq!(second.entries.len(), 200);
    assert!(
        second.continuation.is_none(),
        "the last page must not ask for more"
    );

    // Pages must not overlap, or the UI would show duplicates while scrolling.
    let first_keys: std::collections::HashSet<_> =
        first.entries.iter().map(|e| e.key.clone()).collect();
    assert!(
        second.entries.iter().all(|e| !first_keys.contains(&e.key)),
        "pages overlapped"
    );
}
