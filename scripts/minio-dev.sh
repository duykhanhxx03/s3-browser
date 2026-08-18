#!/usr/bin/env bash
# Starts a local MinIO with the fixture data the tests and the M0 spike expect.
# Usage: scripts/minio-dev.sh [start|stop|reset]
set -euo pipefail

CONTAINER=s3browser-minio
NETWORK=s3b

start() {
  docker network create "$NETWORK" >/dev/null 2>&1 || true

  if docker ps --format '{{.Names}}' | grep -qx "$CONTAINER"; then
    echo "MinIO already running on http://127.0.0.1:9000 (console :9001)"
    return
  fi

  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  docker run -d --name "$CONTAINER" --network "$NETWORK" \
    -p 9000:9000 -p 9001:9001 \
    -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
    minio/minio server /data --console-address ":9001" >/dev/null

  for _ in $(seq 1 30); do
    if curl -sf http://127.0.0.1:9000/minio/health/live >/dev/null; then break; fi
    sleep 1
  done

  seed
  echo "MinIO ready on http://127.0.0.1:9000 (console :9001, minioadmin/minioadmin)"
}

seed() {
  docker run --rm --network "$NETWORK" --entrypoint sh minio/mc -c '
    mc alias set local http://s3browser-minio:9000 minioadmin minioadmin >/dev/null
    mc mb -p local/demo-bucket local/photos-2026 >/dev/null
    for d in reports invoices logs; do
      for i in 1 2 3 4 5; do
        echo "sample $d $i" | mc pipe local/demo-bucket/$d/file-$i.txt >/dev/null
      done
    done
    echo "hello from s3browser" | mc pipe local/demo-bucket/readme.txt >/dev/null
    head -c 3000000 /dev/urandom | mc pipe local/demo-bucket/blob.bin >/dev/null
  ' >/dev/null

  if [ "${LARGE:-0}" = "1" ]; then
    # 1200 keys, so listings cross the 1000-key page boundary and exercise paging.
    docker run --rm --network "$NETWORK" --entrypoint sh minio/mc -c '
      mc alias set local http://s3browser-minio:9000 minioadmin minioadmin >/dev/null
      mkdir -p /tmp/many && cd /tmp/many
      for i in $(seq 1 1200); do printf "obj %04d" $i > file-$(printf "%04d" $i).txt; done
      mc cp --recursive /tmp/many/ local/demo-bucket/many/ >/dev/null 2>&1
    ' >/dev/null
    echo "seeded demo-bucket/many/ with 1200 objects"
  fi
}

# `--large` also seeds a 1200-key prefix for the paging tests.
LARGE=0
for arg in "$@"; do
  [ "$arg" = "--large" ] && LARGE=1
done
export LARGE

case "${1:-start}" in
  start) start ;;
  stop) docker rm -f "$CONTAINER" >/dev/null 2>&1 && echo "stopped" ;;
  reset) docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; start ;;
  *) echo "usage: $0 [start|stop|reset] [--large]" >&2; exit 1 ;;
esac
