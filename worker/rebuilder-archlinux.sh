#!/bin/sh
set -xe
echo "Starting rebuild of ${1} at $(date -u)"
NOCHECK=1 archlinux-repro -o "${REBUILDERD_OUTDIR}" -- "${1}"
