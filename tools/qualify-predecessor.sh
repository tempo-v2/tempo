#!/bin/sh
set -eu

if [ "$#" -ne 4 ]; then
    echo "Usage: tools/qualify-predecessor.sh CANDIDATE PREDECESSOR READ_ONLY_SNAPSHOT PACK_ARTIFACT" >&2
    exit 2
fi
if [ ! -x "$1" ] || [ ! -x "$2" ] || [ ! -f "$3" ] || [ ! -r "$3" ] ||
    [ ! -f "$4" ] || [ ! -r "$4" ]; then
    echo "Provide two executable binaries, a readable SQLite snapshot and a readable pack artifact." >&2
    exit 2
fi
if [ "$(sqlite3 -readonly "$3" 'PRAGMA user_version;')" != 18 ]; then
    echo "The current predecessor replay harness requires a schema-18 snapshot." >&2
    exit 2
fi
tools="$(cd "$(dirname "$0")" && pwd)"
node "$tools/qualify_gap_resolution.cjs" --predecessor-only "$1" "$2" "$3"
node "$tools/qualify_answer_ir.cjs" --predecessor-only "$@"
