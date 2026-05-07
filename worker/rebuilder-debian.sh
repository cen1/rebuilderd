#!/bin/sh
set -xe
echo "Starting rebuild of ${1} at $(LC_ALL=C date -u)"
cd "$(dirname "$1")"
# for production it's useful to call debrebuild with --cache="$directory"
debrebuild --buildresult="${REBUILDERD_OUTDIR}" --cache="${REBUILDERD_CACHE_DIR}" --builder=sbuild+unshare -- "${1}"
