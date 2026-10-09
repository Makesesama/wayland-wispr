#!/bin/sh
# Dev note hook (dev/config.toml → [note] hook): appends each note to
# dev/notes.md (gitignored) under a timestamp heading.
# Try it without the daemon: echo "test note" | dev/note-hook.sh
set -eu
notes="$(dirname "$0")/notes.md"
{
  printf '## %s\n\n' "$(date '+%Y-%m-%d %H:%M:%S')"
  cat
  printf '\n\n'
} >> "$notes"
echo "appended to $notes" >&2
