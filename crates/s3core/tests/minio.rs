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

async fn client_or_skip() -> Option<S3Client> {
    match S3Client::connect(&Profile::minio_local()).await {
        Ok(client) => match client.list_buckets().await {
            Ok(_) => Some(client),
            Err(_) => {
                eprintln!("skipping: MinIO not reachable on 127.0.0.1:9000");
                None
            }
        },
        Err(_) => None,
    }
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
