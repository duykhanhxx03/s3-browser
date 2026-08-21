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

#[tokio::test]
async fn reads_metadata_and_round_trips_tags() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let bucket = "demo-bucket";
    let key = "inspect-tests/doc.txt";
    client
        .put_object(bucket, key, b"inspect me".to_vec())
        .await
        .unwrap();

    let head = client.head_object(bucket, key).await.unwrap();
    assert_eq!(head.size, 10);
    assert!(head.modified_epoch.is_some(), "inspector shows a date");
    assert!(head.etag.is_some());
    // A plain object is readable as-is, so no restore control should appear.
    assert_eq!(
        s3core::restore_state(head.restore.as_deref(), head.storage_class.as_deref()),
        s3core::RestoreState::NotArchived
    );

    // No tags yet.
    assert!(client.object_tags(bucket, key).await.unwrap().is_empty());

    let tags = vec![
        ("env".to_string(), "prod".to_string()),
        ("owner".to_string(), "mai".to_string()),
    ];
    client.set_object_tags(bucket, key, &tags).await.unwrap();
    assert_eq!(client.object_tags(bucket, key).await.unwrap(), tags);

    // Writing a shorter set replaces rather than merges — S3 has no per-tag edit.
    let one = vec![("env".to_string(), "staging".to_string())];
    client.set_object_tags(bucket, key, &one).await.unwrap();
    assert_eq!(client.object_tags(bucket, key).await.unwrap(), one);

    // The empty set deletes the tagging entirely.
    client.set_object_tags(bucket, key, &[]).await.unwrap();
    assert!(client.object_tags(bucket, key).await.unwrap().is_empty());

    client.delete_object(bucket, key).await.unwrap();
}

#[tokio::test]
async fn versions_restore_and_delete_individually() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let bucket = "versioned-bucket";
    if client.list_page(bucket, "", None).await.is_err() {
        eprintln!("skipping: no versioned-bucket; run scripts/minio-dev.sh reset");
        return;
    }
    let key = "versions/notes.txt";
    // A sibling sharing the prefix: ListObjectVersions takes a prefix, not a
    // key, so this would show up as a phantom version without filtering.
    let sibling = "versions/notes.txt.bak";

    client.put_object(bucket, key, b"v1".to_vec()).await.unwrap();
    client.put_object(bucket, key, b"v2".to_vec()).await.unwrap();
    client
        .put_object(bucket, sibling, b"unrelated".to_vec())
        .await
        .unwrap();

    let versions = client.list_versions(bucket, key).await.unwrap();
    assert_eq!(versions.len(), 2, "two puts make two versions: {versions:?}");
    assert!(
        versions.iter().all(|v| v.key == key),
        "a sibling key leaked in: {versions:?}"
    );
    assert!(versions[0].is_latest, "newest must come first");

    // The current bytes are v2; restoring the older version brings v1 back as a
    // new latest, without losing v2 from the history.
    let older = versions
        .iter()
        .find(|v| !v.is_latest)
        .expect("an older version");
    client
        .restore_version(bucket, key, &older.version_id)
        .await
        .unwrap();

    let current = client.get_range(bucket, key, 0..2, None).await.unwrap();
    assert_eq!(current, b"v1", "restore should bring the old bytes back");
    assert_eq!(
        client.list_versions(bucket, key).await.unwrap().len(),
        3,
        "restoring adds a version rather than replacing one"
    );

    // An ordinary delete in a versioned bucket hides rather than removes.
    client.delete_object(bucket, key).await.unwrap();
    let after_delete = client.list_versions(bucket, key).await.unwrap();
    assert!(
        after_delete.iter().any(|v| v.is_delete_marker),
        "deleting should write a delete marker: {after_delete:?}"
    );
    assert_eq!(
        after_delete.iter().filter(|v| !v.is_delete_marker).count(),
        3,
        "the data versions must survive a plain delete"
    );

    // Deleting versions one by one removes data for good.
    for version in &after_delete {
        client
            .delete_version(bucket, key, &version.version_id)
            .await
            .unwrap();
    }
    assert!(client.list_versions(bucket, key).await.unwrap().is_empty());

    for version in client.list_versions(bucket, sibling).await.unwrap() {
        client
            .delete_version(bucket, sibling, &version.version_id)
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn emptying_a_versioned_bucket_clears_hidden_versions_too() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    // A bucket of its own: this test destroys everything in it.
    let bucket = "empty-flow-test";
    client.create_bucket(bucket).await.ok();

    for i in 0..3 {
        let key = format!("doc-{i}.txt");
        client.put_object(bucket, &key, b"a".to_vec()).await.unwrap();
        // A second put makes a second version if the bucket is versioned, and
        // simply overwrites if it is not — either way the flow must cope.
        client.put_object(bucket, &key, b"bb".to_vec()).await.unwrap();
    }
    // Delete one the ordinary way: in a versioned bucket that leaves a delete
    // marker, which is exactly what makes DeleteBucket fail later.
    client.delete_object(bucket, "doc-0.txt").await.unwrap();

    let mut progress = Vec::new();
    let report = client
        .empty_bucket(bucket, |done, seen| progress.push((done, seen)))
        .await
        .unwrap();

    assert!(report.errors.is_empty(), "{:?}", report.errors);
    assert!(report.deleted > 0, "nothing was deleted");
    assert!(!progress.is_empty(), "progress should be reported");

    // Nothing visible, nothing hidden.
    let page = client.list_page(bucket, "", None).await.unwrap();
    assert!(page.entries.is_empty(), "bucket still lists {page:?}");
    for i in 0..3 {
        assert!(
            client
                .list_versions(bucket, &format!("doc-{i}.txt"))
                .await
                .unwrap()
                .is_empty(),
            "doc-{i}.txt still has versions"
        );
    }

    // The real proof: an empty bucket is one S3 will let you delete.
    client
        .delete_bucket(bucket)
        .await
        .expect("DeleteBucket should succeed once every version is gone");
}

#[tokio::test]
async fn sse_headers_reach_the_server_on_both_upload_paths() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let bucket = "demo-bucket";

    // Baseline: no encryption asked for, none reported.
    let plain = "sse-tests/plain.txt";
    client.put_object(bucket, plain, b"x".to_vec()).await.unwrap();
    let head = client.head_object(bucket, plain).await.unwrap();
    assert_eq!(head.encryption, None, "baseline should be unencrypted");
    client.delete_object(bucket, plain).await.unwrap();

    client.set_encryption(s3core::Encryption::Aes256);
    let small = "sse-tests/small.txt";
    let outcome = client.put_object(bucket, small, b"secret".to_vec()).await;

    // A stock MinIO has no KMS backend and refuses server-side encryption. That
    // refusal is itself the evidence this test can get here: the server could
    // not complain about encryption it was never asked for. What stays
    // unverified locally is whether the stored object really comes back
    // encrypted — that needs a KMS-configured server.
    if let Err(error) = &outcome {
        let message = format!("{error:?}");
        assert!(
            message.contains("NotImplemented") || message.contains("KMS"),
            "expected an encryption-specific refusal, got: {message}"
        );
        eprintln!(
            "skipping end-to-end SSE check: this server has no KMS configured \
             (the header was sent and specifically rejected)"
        );
        client.set_encryption(s3core::Encryption::BucketDefault);
        return;
    }

    // A server that does support it must report the encryption back.
    let head = client.head_object(bucket, small).await.unwrap();
    assert_eq!(
        head.encryption.as_deref(),
        Some("AES256"),
        "PutObject should carry SSE-S3"
    );

    // Multipart sets encryption once at create time; the parts inherit it.
    // Getting this wrong is silent — the upload succeeds, just unencrypted.
    let large = "sse-tests/large.bin";
    let upload_id = client.create_multipart_upload(bucket, large).await.unwrap();
    let etag = client
        .upload_part(bucket, large, &upload_id, 1, vec![9u8; 5 * 1024 * 1024])
        .await
        .unwrap();
    client
        .complete_multipart_upload(
            bucket,
            large,
            &upload_id,
            vec![s3core::CompletedPart {
                part_number: 1,
                etag: etag.etag,
                size: 5 * 1024 * 1024,
                checksum_crc32: etag.checksum_crc32,
            }],
        )
        .await
        .unwrap();

    let head = client.head_object(bucket, large).await.unwrap();
    assert_eq!(
        head.encryption.as_deref(),
        Some("AES256"),
        "multipart parts inherit the upload's encryption"
    );

    client.set_encryption(s3core::Encryption::BucketDefault);
    for key in [small, large] {
        client.delete_object(bucket, key).await.unwrap();
    }
}

#[tokio::test]
async fn assume_role_yields_working_temporary_credentials() {
    let Some(_) = client_or_skip().await else {
        return;
    };
    let base = Profile::minio_local();

    // MinIO implements AssumeRole at its own endpoint, and ignores the role ARN
    // for root credentials — it returns a session for the caller instead. That
    // is enough to prove the request is well-formed and the credentials it hands
    // back actually work.
    let request = s3core::sts::AssumeRole {
        role_arn: "arn:aws:iam::000000000000:role/s3browser-test".into(),
        session_name: "s3browser-test".into(),
        duration: Some(Duration::from_secs(900)),
        ..Default::default()
    };

    let credentials = match s3core::sts::assume_role(&base, &request).await {
        Ok(credentials) => credentials,
        Err(error) => {
            eprintln!("skipping: this server does not support AssumeRole: {error}");
            return;
        }
    };

    assert!(!credentials.session_token.is_empty(), "a session needs a token");
    assert!(
        !credentials.expires_within(Duration::from_secs(60)),
        "a 15-minute session should not already be inside a 1-minute margin"
    );

    // The real test: the temporary credentials can actually talk to S3.
    let assumed = s3core::sts::profile_with(&base, &credentials);
    assert_eq!(
        assumed.session_token.as_deref(),
        Some(credentials.session_token.as_str())
    );
    let client = S3Client::connect(&assumed).await.unwrap();
    let buckets = client
        .list_buckets()
        .await
        .expect("assumed credentials should be able to list buckets");
    assert!(
        buckets.iter().any(|b| b == "demo-bucket"),
        "assumed session sees the same buckets: {buckets:?}"
    );

    // And a presigned URL signed with them must carry the session token, or it
    // would be rejected the moment anyone used it.
    let signed = client
        .presign_get("demo-bucket", "readme.txt", Duration::from_secs(300))
        .await
        .unwrap();
    assert!(
        signed.contains("X-Amz-Security-Token"),
        "a URL signed with temporary credentials must include the token: {signed}"
    );
}

#[tokio::test]
async fn acl_reads_grants_and_flags_public_ones() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let bucket = "demo-bucket";
    let key = "acl-tests/doc.txt";
    client.put_object(bucket, key, b"acl".to_vec()).await.unwrap();

    let caps = client.detect_capabilities(bucket).await;
    if !caps.acl.is_usable() {
        eprintln!("skipping: bucket này không bật ACL ({:?})", caps.acl);
        client.delete_object(bucket, key).await.unwrap();
        return;
    }

    let acl = client.object_acl(bucket, key).await.unwrap();
    if !acl.is_meaningful() {
        // MinIO answers the read with a stub and then refuses the write. Saying
        // so beats asserting against placeholder data.
        eprintln!("skipping: server trả ACL rỗng nghĩa, không thực sự hỗ trợ ({acl:?})");
        client.delete_object(bucket, key).await.unwrap();
        return;
    }
    assert!(!acl.owner.is_empty(), "owner should be reported");
    assert!(
        !acl.grants.iter().any(|grant| grant.public),
        "a new object must not be world-readable: {acl:?}"
    );

    // Reading ACLs and writing them are separate permissions, and a provider
    // can offer one without the other — MinIO answers GetBucketAcl but refuses
    // PutObjectAcl. Say which case ran rather than passing silently either way.
    if let Err(error) = client.set_object_acl(bucket, key, "public-read").await {
        eprintln!("skipping the write path: server từ chối PutObjectAcl ({error})");
        client.delete_object(bucket, key).await.unwrap();
        return;
    }

    let acl = client.object_acl(bucket, key).await.unwrap();
    // The whole point of the `public` flag: spotting this without the caller
    // having to know the AWS group URIs by heart.
    assert!(
        acl.grants.iter().any(|grant| grant.public),
        "public-read should show a public grant: {acl:?}"
    );

    // And back, so the flag tracks the change rather than latching.
    client.set_object_acl(bucket, key, "private").await.unwrap();
    let acl = client.object_acl(bucket, key).await.unwrap();
    assert!(!acl.grants.iter().any(|grant| grant.public), "{acl:?}");

    client.delete_object(bucket, key).await.unwrap();
}

/// The bug this catches: uploads used to send no Content-Type at all, and the
/// only place it showed was outside the app — a browser handed a presigned URL
/// for an image downloading it instead of displaying it. Asserting against a
/// real server rather than against the guesser, because the guess being right
/// and the header actually arriving are two different claims.
#[tokio::test]
async fn an_upload_stores_the_content_type_its_name_implies() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let bucket = "demo-bucket";

    // A one-pixel PNG, so the object really is what its name says.
    let png: Vec<u8> = vec![
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52,
    ];
    let key = "content-type-tests/pixel.png";
    client.put_object(bucket, key, png).await.unwrap();
    assert_eq!(
        client.head_object(bucket, key).await.unwrap().content_type.as_deref(),
        Some("image/png")
    );

    let key = "content-type-tests/page.html";
    client
        .put_object(bucket, key, b"<h1>xin chao</h1>".to_vec())
        .await
        .unwrap();
    assert_eq!(
        client.head_object(bucket, key).await.unwrap().content_type.as_deref(),
        Some("text/html")
    );

    // No extension: the app claims nothing and the provider applies its own
    // default. Sending a made-up type here would be worse than sending none.
    let key = "content-type-tests/README";
    client
        .put_object(bucket, key, b"khong co duoi".to_vec())
        .await
        .unwrap();
    let head = client.head_object(bucket, key).await.unwrap();
    assert!(
        head.content_type.as_deref() == Some("application/octet-stream")
            || head.content_type.is_none(),
        "expected the provider default, got {:?}",
        head.content_type
    );

    // Multipart is a separate call and a separate place to forget it. Big
    // uploads are exactly the ones that go this way, so a fix that only covered
    // `PutObject` would leave every large image untyped.
    let key = "content-type-tests/big.jpg";
    let upload_id = client
        .create_multipart_upload(bucket, key)
        .await
        .unwrap();
    // 5 MiB is S3's minimum for any part but the last.
    let part = client
        .upload_part(bucket, key, &upload_id, 1, vec![7u8; 5 * 1024 * 1024])
        .await
        .unwrap();
    client
        .complete_multipart_upload(
            bucket,
            key,
            &upload_id,
            vec![s3core::CompletedPart {
                part_number: 1,
                etag: part.etag,
                checksum_crc32: part.checksum_crc32,
                size: 5 * 1024 * 1024,
            }],
        )
        .await
        .unwrap();
    assert_eq!(
        client.head_object(bucket, key).await.unwrap().content_type.as_deref(),
        Some("image/jpeg")
    );
}

/// Editing headers is a self-copy with `MetadataDirective=REPLACE`, and REPLACE
/// means everything not re-sent is gone. This pins what survives, because the
/// failure mode is silent: the header the user asked for is right, and the
/// storage class or the user metadata they never mentioned has vanished.
#[tokio::test]
async fn rewriting_headers_keeps_what_it_was_not_asked_to_change() {
    let Some(client) = client_or_skip().await else {
        return;
    };
    let bucket = "demo-bucket";
    let key = "header-tests/page.bin";

    client
        .put_object(bucket, key, b"<h1>xin chao</h1>".to_vec())
        .await
        .unwrap();
    // `.bin` guesses nothing useful, which is the case worth fixing by hand.
    let tags = vec![("env".to_string(), "prod".to_string())];
    client.set_object_tags(bucket, key, &tags).await.unwrap();

    client
        .set_object_headers(
            bucket,
            key,
            &s3core::ObjectHeaders {
                content_type: Some("text/html".into()),
                cache_control: Some("public, max-age=3600".into()),
                content_disposition: Some("inline".into()),
            },
        )
        .await
        .unwrap();

    let head = client.head_object(bucket, key).await.unwrap();
    assert_eq!(head.content_type.as_deref(), Some("text/html"));
    assert_eq!(head.cache_control.as_deref(), Some("public, max-age=3600"));
    assert_eq!(head.content_disposition.as_deref(), Some("inline"));
    assert_eq!(head.size, 17, "the bytes are untouched");

    // Tags survive because the tagging directive defaults to COPY. If that ever
    // stops being true, a header edit silently strips a cost-allocation tag.
    assert_eq!(client.object_tags(bucket, key).await.unwrap(), tags);

    // Clearing one is a real edit, not a no-op: `None` means "send no header",
    // and REPLACE makes that stick.
    client
        .set_object_headers(
            bucket,
            key,
            &s3core::ObjectHeaders {
                content_type: Some("text/html".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let head = client.head_object(bucket, key).await.unwrap();
    assert_eq!(head.content_type.as_deref(), Some("text/html"));
    assert_eq!(head.cache_control, None, "cleared, not left behind");

    client.delete_object(bucket, key).await.unwrap();
}
