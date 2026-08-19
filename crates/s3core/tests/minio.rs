//! Integration tests against a local MinIO.
//!
//! Start one with:
//!   docker run -d --name s3browser-minio -p 9000:9000 \
//!     -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
//!     minio/minio server /data
//!
//! Tests skip themselves when nothing is listening, so `cargo test` stays green
//! on a machine without Docker.

use std::time::Duration;

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

#[tokio::test]
async fn copies_moves_and_preserves_the_key_exactly() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let bucket = "demo-bucket";
    // A key with the characters that break an unencoded copy source.
    let src = "copy-tests/a file+name?v=1#x.txt";
    let body = b"copy me".to_vec();
    client.put_object(bucket, src, body.clone()).await.unwrap();

    let dst = "copy-tests/copied.txt";
    client.copy_object(bucket, src, bucket, dst).await.unwrap();

    // Both exist, and the copy has the same bytes.
    assert_eq!(
        client.head_object(bucket, dst).await.unwrap().size as usize,
        body.len()
    );
    assert!(client.head_object(bucket, src).await.is_ok());

    // Move: the destination appears and the source is gone.
    let moved = "copy-tests/moved.txt";
    client.move_object(bucket, dst, bucket, moved).await.unwrap();
    assert!(client.head_object(bucket, moved).await.is_ok());
    assert!(
        client.head_object(bucket, dst).await.is_err(),
        "move must delete the source"
    );

    // Moving onto itself is a no-op, not a copy-then-delete that loses the file.
    client
        .move_object(bucket, moved, bucket, moved)
        .await
        .unwrap();
    assert!(
        client.head_object(bucket, moved).await.is_ok(),
        "a self-move must not delete the object"
    );

    for key in [src, moved] {
        client.delete_object(bucket, key).await.unwrap();
    }
}

#[tokio::test]
async fn multipart_copy_reproduces_the_bytes() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let bucket = "demo-bucket";
    let src = "copy-tests/large-source.bin";

    // Driving the real >5 GiB path would mean moving 5 GiB, so instead the part
    // loop is exercised directly with a small part size. That covers the range
    // arithmetic, which is the part that actually goes wrong.
    let content: Vec<u8> = (0..12 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    client
        .put_object(bucket, src, content.clone())
        .await
        .unwrap();

    let dst = "copy-tests/large-copy.bin";
    // 5 MiB parts over 12 MiB exercises the range arithmetic — the part that
    // actually goes wrong — without moving a real 5 GiB object.
    client
        .copy_object_multipart(bucket, src, bucket, dst, 5 * 1024 * 1024)
        .await
        .unwrap();

    // Read it back and compare byte for byte.
    let read = client
        .get_range(bucket, dst, 0..content.len() as u64, None)
        .await
        .unwrap();
    assert_eq!(read, content, "copied bytes differ from the source");

    for key in [src, dst] {
        client.delete_object(bucket, key).await.unwrap();
    }
}

#[tokio::test]
async fn moves_a_whole_prefix_keeping_its_structure() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let bucket = "demo-bucket";
    let src = "move-tests/trip/";
    let dst = "move-tests/journey/";

    for key in ["readme.txt", "day-1/photo.bin", "day-1/notes/a.txt"] {
        client
            .put_object(bucket, &format!("{src}{key}"), key.as_bytes().to_vec())
            .await
            .unwrap();
    }

    let mut seen = Vec::new();
    let report = client
        .move_prefix(bucket, src, dst, |done, total| seen.push((done, total)))
        .await
        .unwrap();

    assert_eq!(report.moved, 3);
    assert!(report.errors.is_empty(), "{:?}", report.errors);
    // Progress runs from 0 to total, so a bar can start empty and end full.
    assert_eq!(seen.first(), Some(&(0, 3)));
    assert_eq!(seen.last(), Some(&(3, 3)));

    // Nesting is preserved and the source is gone.
    for key in ["readme.txt", "day-1/photo.bin", "day-1/notes/a.txt"] {
        assert!(
            client.head_object(bucket, &format!("{dst}{key}")).await.is_ok(),
            "{key} should have landed under the new prefix"
        );
        assert!(
            client.head_object(bucket, &format!("{src}{key}")).await.is_err(),
            "{key} should be gone from the old prefix"
        );
    }

    for key in ["readme.txt", "day-1/photo.bin", "day-1/notes/a.txt"] {
        client.delete_object(bucket, &format!("{dst}{key}")).await.unwrap();
    }
}

#[tokio::test]
async fn refuses_to_move_a_prefix_into_itself() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    // Walking keys the move is still creating would never terminate.
    let error = client
        .move_prefix("demo-bucket", "a/", "a/b/", |_, _| {})
        .await
        .expect_err("moving a prefix inside itself must be refused");
    assert!(error.to_string().contains("chính nó"), "{error}");
}

/// Fetches a URL with a raw HTTP/1.1 GET. Enough to prove a presigned URL works
/// without credentials, and avoids pulling an HTTP client in just for one test.
fn http_get(url: &str) -> (u16, Vec<u8>) {
    use std::io::{Read, Write};

    let rest = url.strip_prefix("http://").expect("presigned URL over http");
    let (authority, path) = rest.split_once('/').expect("URL has a path");
    let path = format!("/{path}");

    let mut stream = std::net::TcpStream::connect(authority).expect("connect to MinIO");
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();

    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response has a header block");
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .expect("status line");

    (status, raw[head_end + 4..].to_vec())
}

#[tokio::test]
async fn presigned_url_downloads_without_credentials() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let bucket = "demo-bucket";
    let key = "presign-tests/secret.txt";
    let body = b"only reachable with a signature".to_vec();
    client.put_object(bucket, key, body.clone()).await.unwrap();

    // Unsigned, the object must not be readable — otherwise this test would
    // pass even if signing were broken.
    let public = s3core::public_url(&Profile::minio_local(), bucket, key);
    let (status, _) = http_get(&public);
    assert_eq!(status, 403, "the fixture bucket must not be public");

    let signed = client
        .presign_get(bucket, key, Duration::from_secs(300))
        .await
        .unwrap();
    let (status, fetched) = http_get(&signed);
    assert_eq!(status, 200, "presigned URL should serve the object");
    assert_eq!(fetched, body, "presigned URL returned different bytes");

    client.delete_object(bucket, key).await.unwrap();
}

#[tokio::test]
async fn an_expired_signature_is_refused() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let bucket = "demo-bucket";
    let key = "presign-tests/short-lived.txt";
    client
        .put_object(bucket, key, b"blink".to_vec())
        .await
        .unwrap();

    // One second, then wait it out: proves the expiry is real and not decorative.
    let signed = client
        .presign_get(bucket, key, Duration::from_secs(1))
        .await
        .unwrap();
    std::thread::sleep(Duration::from_secs(2));

    let (status, _) = http_get(&signed);
    assert_eq!(status, 403, "an expired signature must stop working");

    client.delete_object(bucket, key).await.unwrap();
}
