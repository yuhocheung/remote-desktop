#!/usr/bin/env bash
# Chunked, resumable downloader for the Flutter Windows SDK zip.
#
# The sandbox network kills any single TCP connection after ~4 MiB of
# transfer (silent stall / early reset). A 1.9 GiB single connection therefore
# can never finish. Workaround: fetch the file in 2 MiB chunks, each small
# enough to complete inside one surviving connection. Failed / short chunks
# are retried (fresh full-range request) many times; stalled connections are
# killed fast via --speed-limit so retries happen promptly.
set -u
cd "$(dirname "$0")"

URL="https://storage.googleapis.com/flutter_infra_release/releases/stable/windows/flutter_windows_3.44.6-stable.zip"
OUT="flutter_windows_3.44.6-stable.zip"
TOTAL=1899929646
CHUNK=2097152            # 2 MiB << 4 MiB survival threshold
PARTS_DIR=".flutter_dl_parts"

mkdir -p "$PARTS_DIR"
NCHUNKS=$(( (TOTAL + CHUNK - 1) / CHUNK ))

echo "TOTAL=$TOTAL CHUNK=$CHUNK NCHUNKS=$NCHUNKS"

fetch_chunk() {
  local idx="$1"
  local start=$(( idx * CHUNK ))
  local end=$(( start + CHUNK - 1 ))
  if [ "$end" -ge "$TOTAL" ]; then end=$(( TOTAL - 1 )); fi
  local want=$(( end - start + 1 ))
  local part="$PARTS_DIR/chunk_$(printf '%04d' "$idx").part"

  for attempt in $(seq 1 40); do
    curl -s --speed-limit 2000 --speed-time 10 --max-time 180 \
         -r "${start}-${end}" -o "$part" "$URL"
    local got
    got=$(stat -c%s "$part" 2>/dev/null || echo 0)
    if [ "$got" -eq "$want" ]; then
      printf 'chunk %s OK (%d bytes)\n' "$idx" "$got"
      return 0
    fi
    # short/zero -> delete and retry fresh
    rm -f "$part"
  done
  echo "FATAL: chunk $idx failed after retries"
  return 2
}

for (( i=0; i<NCHUNKS; i++ )); do
  if ! fetch_chunk "$i"; then exit 2; fi
done

echo "All chunks fetched. Concatenating..."
cat "$PARTS_DIR"/chunk_*.part > "$OUT"
finalsize=$(stat -c%s "$OUT" 2>/dev/null || echo 0)
echo "Final size: $finalsize (expected $TOTAL)"
if [ "$finalsize" -eq "$TOTAL" ]; then
  echo "DOWNLOAD_COMPLETE"
  rm -rf "$PARTS_DIR"
else
  echo "SIZE_MISMATCH"
  exit 3
fi
