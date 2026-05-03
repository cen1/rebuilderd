#!/usr/bin/env bash
set -eo pipefail

# rebuilder-freebsd.sh — rebuild a FreeBSD port for rebuilderd-worker.
#
# Useful links:
#   https://pkg-status.freebsd.org/
#   https://api.github.com/repos/freebsd/freebsd-ports/commits?sha=main&per_page=1000
#
# Invocation:
#   rebuilderd-worker calls this script with a single binary .pkg file as $1.
#   That package is used only to identify the port *origin* (e.g. net/ethersync).
#   A FreeBSD port can produce multiple split packages from a single build run
#   (e.g. java/openjdk26 → openjdk26, openjdk26-jre, openjdk26-jre-headless).
#   rebuilderd schedules one queue entry per port origin and registers every split
#   package as a separate artifact on that entry.  This script therefore builds
#   the entire port with poudriere and collects *all* resulting .pkg files into
#   REBUILDERD_OUTDIR.  rebuilderd-worker then compares each rebuilt package
#   against its corresponding artifact URL and reports an individual result.
#
# Arguments:
#   $1  Path to a representative .pkg file for the port to rebuild.
#       The file is unpacked to read its +MANIFEST, which provides:
#         - origin            (e.g. "java/openjdk26")  — drives the poudriere build
#         - name / version    — used for logging only
#         - ports_top_git_hash — commit checked out for the ports tree
#         - build_timestamp   — recorded for SOURCE_DATE_EPOCH (future use)
#         - arch              — selects the appropriate jail and ports tree
#
# Environment variables (required):
#   REBUILDERD_OUTDIR       Directory into which all rebuilt .pkg files are moved.
#
# Environment variables (optional):
#   REBUILDERD_WORKER_NAME  Logical name used to derive jail and ports-tree names
#                           (default: output of `hostname -s`).  Dashes are replaced
#                           with underscores because poudriere disallows dashes in
#                           ports-tree names.
#   JAIL                    Override the poudriere jail name entirely.
#                           Default: rebuilderd-<worker>-<major>-<arch>
#   PORTS_TREE              Override the poudriere ports-tree name entirely.
#                           Default: <worker> (same as WORKER_NAME_SAFE)
#
# poudriere.conf requirements:
#   PRESERVE_TIMESTAMP=yes  — required for timestamp reproducibility
#   PKG_REPRODUCIBLE=yes    — required for reproducible package metadata
#
# Manual testing example (port with multiple split packages):
#   fetch https://pkg.freebsd.org/FreeBSD:14:amd64/latest/All/openjdk26-26.0.0.pkg
#   mkdir -p /tmp/out
#   REBUILDERD_OUTDIR=/tmp/out \
#     JAIL=rebuilderd-test-14-amd64 \
#     PORTS_TREE=workertest \
#     ./rebuilder-freebsd.sh openjdk26-26.0.0.pkg
#   # All split packages (openjdk26, openjdk26-jre, openjdk26-jre-headless, …)
#   # should appear in /tmp/out/.
#
# Manual testing example (single-binary port):
#   fetch https://pkg.freebsd.org/FreeBSD:14:amd64/latest/All/hello-2.12.2.pkg
#   REBUILDERD_OUTDIR=/tmp/out JAIL=rebuilderd-1-14-amd64 PORTS_TREE=worker1 \
#     ./rebuilder-freebsd.sh hello-2.12.2.pkg

echo "Starting rebuild of ${1} at $(date -u)"

# Cleanup function for unexpected exits
cleanup_on_exit() {
  if [ -n "$JAIL" ] && [ -n "$PORTS_TREE" ]; then
    JAIL_NAME="${JAIL}-${PORTS_TREE}"
    if jls -j "$JAIL_NAME" jid 2>/dev/null >/dev/null; then
      echo "Cleaning up: stopping jail $JAIL_NAME..."
      poudriere jail -k -j "$JAIL" -p "$PORTS_TREE" 2>/dev/null || poudriere jail -k -j "$JAIL" 2>/dev/null || true
    fi
  fi
  rm -f /tmp/poudriere_output.log
}

# Set trap to cleanup on exit (except normal exit 0)
trap cleanup_on_exit EXIT

# Ensure output directory is set
if [ -z "$REBUILDERD_OUTDIR" ]; then
  echo "Error: REBUILDERD_OUTDIR is not set." >&2
  exit 1
fi

# Input package is the first argument
INPUT_PKG="$1"
if [ ! -f "$INPUT_PKG" ]; then
  echo "Error: Input package '$INPUT_PKG' not found." >&2
  exit 1
fi

# 1. Identify the port origin.
# Unpack the representative input .pkg and read its +MANIFEST to extract the
# 'origin' field (and other metadata).  The input package itself is not built —
# it is only used as a source of metadata to drive the poudriere build.
PKG_DIR=$(mktemp -d)
# Use bsdtar as it's the default on FreeBSD
bsdtar -xf "$INPUT_PKG" -C "$PKG_DIR"
MANIFEST_PATH=$(find "$PKG_DIR" -name "+MANIFEST")

if [ ! -f "$MANIFEST_PATH" ]; then
    echo "Error: +MANIFEST not found in '$INPUT_PKG'." >&2
    rm -rf "$PKG_DIR"
    exit 1
fi

echo "=== Package Manifest ==="
cat "$MANIFEST_PATH"
echo "========================"
echo ""

ORIGIN=$(grep -o '"origin":"[^"]*"' "$MANIFEST_PATH" | head -1 | cut -d ':' -f 2 | tr -d '"')
PKG_NAME=$(grep -o '"name":"[^"]*"' "$MANIFEST_PATH" | head -1 | cut -d ':' -f 2 | tr -d '"')
PKG_VERSION=$(grep -o '"version":"[^"]*"' "$MANIFEST_PATH" | head -1 | cut -d ':' -f 2 | tr -d '"')

# Extract annotations from manifest
PORTS_GIT_HASH=$(grep -o '"ports_top_git_hash":"[^"]*"' "$MANIFEST_PATH" | head -1 | sed 's/.*"ports_top_git_hash":"\([^"]*\)".*/\1/')
BUILD_TIMESTAMP=$(grep -o '"build_timestamp":"[^"]*"' "$MANIFEST_PATH" | head -1 | sed 's/.*"build_timestamp":"\([^"]*\)".*/\1/')

echo "Detected package: $PKG_NAME-$PKG_VERSION (origin: $ORIGIN)"
echo "Ports tree commit: $PORTS_GIT_HASH"
echo "Build timestamp: $BUILD_TIMESTAMP"

# Extract FreeBSD architecture info (format: FreeBSD:14:amd64)
PKG_ARCH_FULL=$(pkg info -F "$INPUT_PKG" | grep '^Architecture' | awk '{print $3}')
FREEBSD_MAJOR=$(echo "$PKG_ARCH_FULL" | cut -d ':' -f 2)
PKG_ARCH=$(echo "$PKG_ARCH_FULL" | cut -d ':' -f 3)

# Noarch packages use "*" - just build them on amd64
[ "$PKG_ARCH" = "*" ] && PKG_ARCH="amd64"

# Extract FreeBSD version to determine RELEASE version
FREEBSD_VERSION_NUM=$(pkg info -F "$INPUT_PKG" | grep 'FreeBSD_version' | awk '{print $2}')

# For noarch packages (arch=*), FreeBSD_version may not be present
# In that case, default to MAJOR.0-RELEASE
if [ -z "$FREEBSD_VERSION_NUM" ]; then
  echo "Note: FreeBSD_version not found (likely noarch package), using ${FREEBSD_MAJOR}.0-RELEASE"
  FREEBSD_MINOR=0
else
  # Convert version number to RELEASE format (e.g., 1403000 -> 14.3-RELEASE)
  # Format: MAJOR*100000 + MINOR*1000
  FREEBSD_MINOR=$((FREEBSD_VERSION_NUM / 1000 % 100))
fi
FREEBSD_RELEASE="${FREEBSD_MAJOR}.${FREEBSD_MINOR}-RELEASE"

rm -rf "$PKG_DIR"

if [ -z "$ORIGIN" ]; then
  echo "Error: Could not determine port origin from '$INPUT_PKG'." >&2
  exit 1
fi

# TODO: dynamic jail and ports tree naming for multiple worker per server support
# We currently assume single worker for simplicity
# Auto jail and ports tree setup
# Use worker name from environment, fallback to hostname if not set
WORKER_NAME="${REBUILDERD_WORKER_NAME:-$(hostname -s)}"
# Sanitize worker name by replacing dashes with underscores (poudriere doesn't allow dashes in ports tree names)
WORKER_NAME_SAFE=$(echo "$WORKER_NAME" | tr '-' '_')
# Allow JAIL and PORTS_TREE to be overridden via environment
if [ -z "$JAIL" ]; then
  JAIL="rebuilderd-${WORKER_NAME_SAFE}-${FREEBSD_MAJOR}-${PKG_ARCH}"
fi
if [ -z "$PORTS_TREE" ]; then
  PORTS_TREE="${WORKER_NAME_SAFE}"
fi

echo "Using jail: $JAIL (FreeBSD $FREEBSD_RELEASE)"

# Check if jail exists, create if not
poudriere jail -l -q > /tmp/jail_list.txt 2>&1 || true
if ! grep -q "^${JAIL} " /tmp/jail_list.txt; then
  echo "Jail '$JAIL' not found, creating..."
  poudriere jail -c -j "$JAIL" -v "$FREEBSD_RELEASE" -a "$PKG_ARCH"
  if [ $? -ne 0 ]; then
    echo "Error: Failed to create jail '$JAIL'." >&2
    rm -f /tmp/jail_list.txt
    exit 1
  fi
  echo "Jail '$JAIL' created successfully."
else
  echo "Jail '$JAIL' already exists."
fi
rm -f /tmp/jail_list.txt

# Configure jail to use 'latest' pkg repository (not 'quarterly')
JAIL_PKG_REPO_DIR="/usr/local/poudriere/jails/${JAIL}/usr/local/etc/pkg/repos"
JAIL_PKG_REPO_CONF="${JAIL_PKG_REPO_DIR}/FreeBSD.conf"

echo "Configuring jail '$JAIL' to use 'latest' pkg repository..."
mkdir -p "$JAIL_PKG_REPO_DIR"
cat > "$JAIL_PKG_REPO_CONF" << 'EOF'
FreeBSD: {
  url: "pkg+http://pkg.FreeBSD.org/${ABI}/latest",
  enabled: yes
}
EOF

if [ -f "$JAIL_PKG_REPO_CONF" ]; then
  echo "Jail pkg repository configured to use 'latest'."
else
  echo "Warning: Failed to create jail pkg repository config." >&2
fi

# Check if ports tree exists, create if not
poudriere ports -l -q > /tmp/ports_list.txt 2>&1 || true
if ! grep -q "^${PORTS_TREE} " /tmp/ports_list.txt; then
  echo "Ports tree '$PORTS_TREE' not found, creating with full git history..."
  poudriere ports -c -p "$PORTS_TREE" -m git+https -D
  if [ $? -ne 0 ]; then
    echo "Error: Failed to create ports tree '$PORTS_TREE'." >&2
    rm -f /tmp/ports_list.txt
    exit 1
  fi
  echo "Ports tree '$PORTS_TREE' created successfully."
else
  echo "Ports tree '$PORTS_TREE' already exists."
fi
rm -f /tmp/ports_list.txt

# Create necessary directories
mkdir -p /usr/ports/distfiles
mkdir -p /tmp/rebuilderd-output
mkdir -p /usr/local/poudriere/data/logs/bulk/.html
mkdir -p /usr/local/poudriere/data/logs/bulk/.html/assets

# Validate we have the git hash
if [ -z "$PORTS_GIT_HASH" ]; then
  echo "Error: Could not extract ports_top_git_hash from package manifest." >&2
  exit 1
fi

# Detect hash length and configure git to match
# Fixed in https://github.com/freebsd/poudriere/issues/1286
# Left as documentation
# HASH_LENGTH=${#PORTS_GIT_HASH}
# echo "Configuring git to use $HASH_LENGTH-char hashes to match original package"
# git config --global core.abbrev "$HASH_LENGTH"

# 2. Prepare build environment and build
# This script assumes that poudriere is installed and configured.
# A poudriere jail and a ports tree must be set up.
# The rebuilderd user needs privileges to run poudriere.

# 3. Checkout the correct ports tree commit
# Rebuilderd daemon will sort packages by ports tree commit timestamp
# and send them one by one. This way we reduce jumping around in time
# and simulate the upstream CI build process more closely which hopefully improves rebuildability.
# It also means less rebuilds of the same dependency packages.
# Failed retried packages obviously break that logic but we have to live with that for now.
PORTS_DIR="/usr/local/poudriere/ports/${PORTS_TREE}"
if [ ! -d "$PORTS_DIR/.git" ]; then
  echo "Error: Poudriere ports tree '$PORTS_DIR' is not a git repository." >&2
  exit 1
fi

echo "Checking out ports tree commit $PORTS_GIT_HASH in $PORTS_DIR"

# Unshallow if it's a shallow clone (no-op if already full)
git -C "$PORTS_DIR" fetch --unshallow 2>/dev/null || git -C "$PORTS_DIR" fetch --all

# Checkout the specific commit
if ! git -C "$PORTS_DIR" checkout "$PORTS_GIT_HASH" 2>/dev/null; then
  echo "Error: Could not checkout commit $PORTS_GIT_HASH" >&2
  exit 1
fi

# Set SOURCE_DATE_EPOCH to the commit time for reproducible builds
# This doesn't really do anything to improve reproducibility at the moment because neither upstream CI
# nor poudriere set this but hopefully once upstream and poudriere starts setting it to something we're ready.
# Ticket: https://github.com/freebsd/poudriere/issues/1287
SOURCE_DATE_EPOCH=$(git -C "$PORTS_DIR" show -s --format=%ct "$PORTS_GIT_HASH")
if [ -n "$SOURCE_DATE_EPOCH" ]; then
  export SOURCE_DATE_EPOCH
  echo "Set SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH (commit time of $PORTS_GIT_HASH)"
else
  echo "Warning: Could not get commit time from git" >&2
fi

# Function to stop jail and wait for it to fully stop
stop_jail_if_running() {
  # Check for jail with ports tree suffix (poudriere uses jail-portstree format when running)
  JAIL_NAME="${JAIL}-${PORTS_TREE}"
  if jls -j "$JAIL_NAME" jid 2>/dev/null >/dev/null; then
    echo "Jail ${JAIL_NAME} is running — stopping it..."
    poudriere jail -k -j "${JAIL}" -p "${PORTS_TREE}" 2>/dev/null || poudriere jail -k -j "${JAIL}" 2>/dev/null || true

    # Wait up to 30 seconds for jail to stop
    WAIT_COUNT=0
    while jls -j "$JAIL_NAME" jid 2>/dev/null >/dev/null; do
      if [ $WAIT_COUNT -ge 30 ]; then
        echo "Warning: Jail did not stop after 30 seconds, forcing cleanup..."
        jls -j "$JAIL_NAME" jid 2>/dev/null | xargs -r jexec {} killall -9 2>/dev/null || true
        sleep 2
        break
      fi
      echo "Waiting for jail to stop... ($WAIT_COUNT/30)"
      sleep 1
      WAIT_COUNT=$((WAIT_COUNT + 1))
    done

    if ! jls -j "$JAIL_NAME" jid 2>/dev/null >/dev/null; then
      echo "Jail ${JAIL} stopped successfully."
    fi
  fi
}

# Check if the jail is running (can stay up when stopping the service)
stop_jail_if_running

# Clean up any leftover build state from previous failed builds
BUILDING_DIR="/usr/local/poudriere/data/packages/${JAIL}-${PORTS_TREE}/.building"
if [ -d "$BUILDING_DIR" ]; then
  echo "Cleaning up leftover build state from $BUILDING_DIR..."
  rm -rf "$BUILDING_DIR"
fi

# Configure git in the jail to use 10-char abbreviated hashes
# Only affects non-reproducability of MANIFEST which we ignore anyway at the moment.
# Fixed in https://github.com/freebsd/poudriere/issues/1286 but leaving for documentation.
# JAIL_ROOT="/usr/local/poudriere/jails/${JAIL}"
# if [ -d "$JAIL_ROOT" ]; then
#   echo "Configuring git in jail to use 10-char hashes..."
#   mkdir -p "$JAIL_ROOT/root/.config/git"
#   cat > "$JAIL_ROOT/root/.config/git/config" << EOF
# [core]
# 	abbrev = 10
# EOF
# fi

# The following command will build the port and its dependencies.
# The official build flags should be configured in poudriere.conf, for example:

# lang/rust OOM prevention
#
# rust-installer's "combine" step decompresses 9+ xz tarballs (rustc, cargo,
# rust-analyzer, clippy, …) into the WRKDIR and re-combines them into one
# archive.  When WRKDIR lives on tmpfs this can spike RAM usage to 8-10 GB and
# trigger the OOM killer — even after compilation itself succeeds.
#
# Fix: add the following to poudriere.conf on each worker host:
#   TMPFS_BLACKLIST="rust"
#   TMPFS_BLACKLIST_TMPDIR=/path/to/scratch
#
# Reference: https://forums.freebsd.org/threads/lang-rust-poudriere-build-oom-killer.87803/
POUDRIERE_BULK_ARGS=""

# Run poudriere bulk and handle "jail already running" error
# Use tee to show output in real-time while capturing to file
set +e
poudriere bulk -j "$JAIL" -p "$PORTS_TREE" $POUDRIERE_BULK_ARGS "$ORIGIN" 2>&1 | tee /tmp/poudriere_output.log
POUDRIERE_EXIT=${PIPESTATUS[0]}
set -e

if [ "$POUDRIERE_EXIT" -ne 0 ]; then
  # Check if the error was "jail already running"
  if grep -qi "jail already running" /tmp/poudriere_output.log; then
    echo "Error: Jail already running detected. Stopping the jail..."
    stop_jail_if_running
  fi
  rm -f /tmp/poudriere_output.log
  echo "Error: Poudriere build failed with exit code $POUDRIERE_EXIT" >&2
  exit 1
fi

rm -f /tmp/poudriere_output.log
echo "Build completed successfully."

# Output the poudriere build log for rebuilderd to capture
POUDRIERE_LOG_DIR="/usr/local/poudriere/data/logs/bulk/${JAIL}-${PORTS_TREE}/latest/logs"
BUILD_LOG_PATH="${POUDRIERE_LOG_DIR}/${ORIGIN}.log"

if [ -f "$BUILD_LOG_PATH" ]; then
    echo ""
    echo "=== Poudriere Build Log ==="
    cat "$BUILD_LOG_PATH"
    echo "==========================="
    echo ""
else
    echo "Warning: Poudriere build log not found at: $BUILD_LOG_PATH" >&2
fi

# 4. Collect all rebuilt packages for this port origin.
#
# Why collect multiple packages?
#   poudriere bulk builds an entire port in one run, producing *all* split packages
#   at once (e.g. openjdk26, openjdk26-jre, openjdk26-jre-headless).  rebuilderd
#   schedules one queue entry per port origin and records each binary .pkg as a
#   separate artifact on that entry.  This script must therefore output every split
#   package so that rebuilderd-worker can diff each one against its artifact URL
#   and report an individual GOOD/BAD result per binary.
#
# Detection strategy:
#   Iterate over every .pkg in poudriere's .latest/All directory.  Use `pkg info -F`
#   to read the embedded origin field from each package.  Move only the files whose
#   origin matches $ORIGIN to REBUILDERD_OUTDIR.  Finding zero matching packages is
#   treated as a build error.
POUDRIERE_PKG_DIR="/usr/local/poudriere/data/packages/${JAIL}-${PORTS_TREE}/.latest/All"

echo "Collecting all packages for origin '$ORIGIN'..."
pkg_count=0
for pkg_file in "$POUDRIERE_PKG_DIR"/*.pkg; do
    [ -f "$pkg_file" ] || continue
    file_origin=$(pkg info -F "$pkg_file" | awk '/^Origin/ {print $2}')
    if [ "$file_origin" = "$ORIGIN" ]; then
        mv "$pkg_file" "$REBUILDERD_OUTDIR/"
        echo "  Moved: $(basename "$pkg_file")"
        pkg_count=$((pkg_count + 1))
    fi
done

if [ "$pkg_count" -eq 0 ]; then
    echo "Error: Could not find any packages for origin '$ORIGIN' in '$POUDRIERE_PKG_DIR'." >&2
    exit 1
fi

echo ""
echo "Build complete for '$ORIGIN' ($pkg_count package(s))"
exit 0