//! Leaves a real orphaned multipart upload behind, for exercising the cleanup UI
//! against a local MinIO. There is no `mc` command for this — abandoning an
//! upload needs the raw S3 calls — so it lives here as a dev fixture tool.
//!
//! Usage: cargo run -p s3core --example seed_orphan [bucket] [key]
use s3core::{Profile, S3Client};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let bucket = args.next().unwrap_or_else(|| "demo-bucket".into());
    let key = args.next().unwrap_or_else(|| "manual/orphan.bin".into());

    let client = S3Client::connect(&Profile::minio_local()).await?;
    let upload_id = client.create_multipart_upload(&bucket, &key).await?;
    // One part is enough to make it billable and visible to ListMultipartUploads.
    client
        .upload_part(&bucket, &key, &upload_id, 1, vec![7u8; 5 * 1024 * 1024])
        .await?;

    println!("left an orphan at s3://{bucket}/{key} (upload_id={upload_id})");
    Ok(())
}
