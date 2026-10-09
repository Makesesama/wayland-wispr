#!/usr/bin/env bash
# Regenerate nix/models.json from the whisper.cpp model repo on Hugging Face.
# Pins every ggml model to the current repo commit; hashes come from the
# LFS metadata, so nothing is downloaded. Needs curl and jq.
set -euo pipefail

repo=ggerganov/whisper.cpp
api=https://huggingface.co/api/models/$repo
out=$(dirname "$0")/models.json

rev=$(curl -fsSL "$api" | jq -r .sha)
curl -fsSL "$api/tree/$rev" | jq --arg repo "$repo" --arg rev "$rev" '
  {
    repo: $repo,
    rev: $rev,
    models: [
      .[]
      | select(.lfs != null and (.path | test("^ggml-.*\\.bin$")))
      | {
          key: (.path | ltrimstr("ggml-") | rtrimstr(".bin")),
          value: { file: .path, sha256: .lfs.oid, size: .size }
        }
    ] | from_entries
  }' >"$out"
echo "wrote $out (rev $rev)"
