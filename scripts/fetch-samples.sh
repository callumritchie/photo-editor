#!/usr/bin/env bash
# Download the CC0 sample raw files listed in fixtures/samples.json into fixtures/raw/,
# verifying each file's SHA-256. Files are never committed (see .gitignore).
# Usage: scripts/fetch-samples.sh [sample-id ...]   (no args = all samples)
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p fixtures/raw

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

python3 - "$@" <<'EOF' | while IFS=$'\t' read -r id file url sha; do
import json, sys
want = set(sys.argv[1:])
for s in json.load(open("fixtures/samples.json"))["samples"]:
    if not want or s["id"] in want:
        print("\t".join([s["id"], s["file"], s["url"], s["sha256"]]))
EOF
  dest="fixtures/raw/$file"
  if [[ -f "$dest" && "$(sha256 "$dest")" == "$sha" ]]; then
    echo "ok      $id"; continue
  fi
  echo "fetch   $id"
  curl -fsSL --retry 3 -o "$dest.part" "$url"
  got="$(sha256 "$dest.part")"
  if [[ "$got" != "$sha" ]]; then
    rm -f "$dest.part"; echo "FAILED  $id: sha256 $got != $sha" >&2; exit 1
  fi
  mv "$dest.part" "$dest"
  echo "ok      $id"
done
