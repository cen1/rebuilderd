#!/bin/sh
set -xe
echo "Starting rebuild of ${1} at $(LC_ALL=C date -u)"
cd "$(dirname "$1")"

BUILDINFO="${1}"

# debrebuild passes DEB_BUILD_OPTIONS from the buildinfo into the sbuild chroot
# via --env-set, so we must modify a copy of the buildinfo rather than the env.
# Strip the PGP cleartext signature (invalidated by our edit) and cap parallel.
MAX_PARALLEL=32
parallel=$(sed -n '/^Environment:/,/^[^ ]/{/DEB_BUILD_OPTIONS=/p}' "$BUILDINFO" \
    | grep -o 'parallel=[0-9]*' | cut -d= -f2)
if [ -n "$parallel" ] && [ "$parallel" -gt "$MAX_PARALLEL" ]; then
    echo "Capping parallel jobs from $parallel to $MAX_PARALLEL to avoid OOM"
    TMPBUILDINFO=$(mktemp --suffix=.buildinfo)
    trap "rm -f \"$TMPBUILDINFO\"" EXIT
    awk '
        /^-----BEGIN PGP SIGNED MESSAGE-----/ { in_pgp=1; next }
        in_pgp && !body && /^$/ { body=1; next }
        in_pgp && !body { next }
        /^-----BEGIN PGP SIGNATURE-----/ { exit }
        { print }
    ' "$BUILDINFO" | sed "s/parallel=$parallel/parallel=$MAX_PARALLEL/" > "$TMPBUILDINFO"
    BUILDINFO="$TMPBUILDINFO"
fi

# for production it's useful to call debrebuild with --cache="$directory"
debrebuild --buildresult="${REBUILDERD_OUTDIR}" --cache="${REBUILDERD_CACHE_DIR}" --builder=sbuild+unshare -- "$BUILDINFO"
