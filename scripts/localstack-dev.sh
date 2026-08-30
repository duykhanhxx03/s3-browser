#!/usr/bin/env bash
# Starts a local S3 (LocalStack) with fixture data the live tests expect.
# Usage: scripts/localstack-dev.sh [start|stop|reset|seed|status]
#
# Why this exists beside minio-dev.sh: MinIO has no archived storage classes at
# all. Measured against minio/minio, a PUT carrying GLACIER, DEEP_ARCHIVE,
# GLACIER_IR or STANDARD_IA is refused outright with InvalidStorageClass — so
# the storage-temperature feature, which turns entirely on telling those apart,
# cannot be exercised against it even a little. LocalStack stores and returns
# them, which is what lets `temperature_of` be checked against a real S3 API
# instead of against an assumption.
#
# Pinned to 3.8 deliberately. From 2026.x LocalStack refuses to start without a
# registered account even for the free services, and S3 is all this needs.
set -euo pipefail

CONTAINER=s3browser-localstack
IMAGE=localstack/localstack:3.8
ENDPOINT=http://127.0.0.1:4566
BUCKET=s3browser-dev

start() {
  if docker ps --format '{{.Names}}' | grep -qx "$CONTAINER"; then
    echo "LocalStack already running on $ENDPOINT"
    return
  fi
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  docker run -d --name "$CONTAINER" -p 4566:4566 \
    -e SERVICES=s3 -e DEBUG=0 "$IMAGE" >/dev/null

  for _ in $(seq 1 60); do
    if curl -sf "$ENDPOINT/_localstack/health" >/dev/null; then
      echo "LocalStack up on $ENDPOINT"
      seed
      return
    fi
    sleep 2
  done
  echo "LocalStack did not become healthy" >&2
  exit 1
}

# The fixtures are shaped like a real bucket rather than evenly: one prefix
# holding most of the bytes, a couple of real ones, and a long tail. An even
# spread would let a treemap that mishandles dominant prefixes still look fine.
seed() {
  python3 - "$ENDPOINT" "$BUCKET" <<'PY'
import sys, random, boto3
endpoint, bucket = sys.argv[1], sys.argv[2]
s3 = boto3.client("s3", endpoint_url=endpoint, aws_access_key_id="test",
                  aws_secret_access_key="test", region_name="us-east-1")
try:
    s3.create_bucket(Bucket=bucket)
except Exception:
    pass

random.seed(7)
plan = [("logs/2024/", 120, 40_000), ("logs/2025/", 60, 30_000),
        ("media/", 12, 900_000), ("configs/", 8, 400), ("tiny/", 25, 12)]
for prefix, count, avg in plan:
    for i in range(count):
        size = max(1, int(random.gauss(avg, avg * 0.3)))
        s3.put_object(Bucket=bucket, Key=f"{prefix}file-{i:04}.bin", Body=b"x" * size)

for key, cls in [("archive/2019-cold.zip", "GLACIER"),
                 ("archive/2018-deep.zip", "DEEP_ARCHIVE"),
                 ("archive/instant.zip", "GLACIER_IR"),
                 ("archive/infrequent.zip", "STANDARD_IA")]:
    s3.put_object(Bucket=bucket, Key=key, Body=b"y" * 5000, StorageClass=cls)

n = s3.list_objects_v2(Bucket=bucket)["KeyCount"]
print(f"seeded {bucket}: {n} objects")
PY
}

stop() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; echo "stopped"; }
reset() { stop; start; }

status() {
  if curl -sf "$ENDPOINT/_localstack/health" >/dev/null; then
    echo "up on $ENDPOINT"
  else
    echo "not running"
  fi
}

case "${1:-start}" in
  start) start ;;
  stop) stop ;;
  reset) reset ;;
  seed) seed ;;
  status) status ;;
  *) echo "usage: $0 [start|stop|reset|seed|status]" >&2; exit 2 ;;
esac
