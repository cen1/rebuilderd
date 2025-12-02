#!/usr/bin/env bash
set -xeo pipefail

# https://pkg-status.freebsd.org/
# https://api.github.com/repos/freebsd/freebsd-ports/commits?sha=main&per_page=1000
# rebuilder-freebsd.sh: A script to rebuild FreeBSD packages for rebuilderd.
# This script is invoked by the rebuilderd-worker.
# fetch https://pkg.freebsd.org/FreeBSD:14:amd64/latest/All/hello-2.12.2.pkg
# REBUILDERD_OUTDIR=/tmp/out JAIL=rebuilderd-1-14-amd64 PORTS_TREE=worker1 ./rebuilder-freebsd.sh hello-2.12.2.pkg

# Arguments:
#   $1: Path to the .txz package to rebuild.

# Environment variables:
#   REBUILDERD_OUTDIR: Directory to place the rebuilt package in.

# MUST SET PRESERVE_TIMESTAMP=yes in /usr/local/etc/poudriere.conf
# MUST SET PKG_REPRODUCIBLE=yes

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

# 1. Identify the port origin
# Unpack the input .txz and read its +MANIFEST to get the 'origin' field.
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
# Convert version number to RELEASE format (e.g., 1403000 -> 14.3-RELEASE)
# Format: MAJOR*100000 + MINOR*1000
FREEBSD_MINOR=$((FREEBSD_VERSION_NUM / 1000 % 100))
FREEBSD_RELEASE="${FREEBSD_MAJOR}.${FREEBSD_MINOR}-RELEASE"

rm -rf "$PKG_DIR"

if [ -z "$ORIGIN" ]; then
  echo "Error: Could not determine port origin from '$INPUT_PKG'." >&2
  exit 1
fi

# Auto jail and ports tree setup
# Use worker name from environment, fallback to hostname if not set
WORKER_NAME="${REBUILDERD_WORKER_NAME:-$(hostname -s)}"
# Allow JAIL and PORTS_TREE to be overridden via environment
if [ -z "$JAIL" ]; then
  JAIL="rebuilderd-${WORKER_NAME}-${FREEBSD_MAJOR}-${PKG_ARCH}"
fi
# Use separate ports tree per worker to avoid git checkout conflicts
if [ -z "$PORTS_TREE" ]; then
  PORTS_TREE="worker${WORKER_NAME}"
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
HASH_LENGTH=${#PORTS_GIT_HASH}
echo "Configuring git to use $HASH_LENGTH-char hashes to match original package"
git config --global core.abbrev "$HASH_LENGTH"

# 2. Prepare build environment and build
# This script assumes that poudriere is installed and configured.
# A poudriere jail and a ports tree must be set up.
# The rebuilderd user needs privileges to run poudriere (e.g., via sudo).
#
# Example poudriere setup:
#   poudriere jail -c -j 14.2-RELEASE -v 14.2-RELEASE
#   poudriere ports -c -p main -m git

# 3. Checkout the correct ports tree commit
# Checkout the specific git commit in the ports tree
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
JAIL_ROOT="/usr/local/poudriere/jails/${JAIL}"
if [ -d "$JAIL_ROOT" ]; then
  echo "Configuring git in jail to use 10-char hashes..."
  mkdir -p "$JAIL_ROOT/root/.config/git"
  cat > "$JAIL_ROOT/root/.config/git/config" << EOF
[core]
	abbrev = 10
EOF
fi

# The following command will build the port and its dependencies.
# The official build flags should be configured in poudriere.conf, for example:
#   ALLOW_MAKE_JOBS=yes
#   PREPARE_PARALLEL_JOBS=4
#   MAKE_JOBS_NUMBER=4
# Also, respect flags like WITH_DOCS=off, etc. by configuring them in make.conf.

# Run poudriere bulk and handle "jail already running" error
# Use tee to show output in real-time while capturing to file
set +e
poudriere bulk -j "$JAIL" -p "$PORTS_TREE" "$ORIGIN" 2>&1 | tee /tmp/poudriere_output.log
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

# 4. Output the rebuilt package
# poudriere places the built packages in a directory structure.
# We need to find the correct package and move it to REBUILDERD_OUTDIR.
POUDRIERE_PKG_DIR="/usr/local/poudriere/data/packages/${JAIL}-${PORTS_TREE}/.latest/All"
BUILT_PKG=$(find "$POUDRIERE_PKG_DIR" -name "${PKG_NAME}-${PKG_VERSION}*.pkg")

if [ -z "$BUILT_PKG" ] || [ ! -f "$BUILT_PKG" ]; then
    echo "Error: Could not find the built package for origin '$ORIGIN'." >&2
    exit 1
fi

# Move the package to the output directory.
mv "$BUILT_PKG" "$REBUILDERD_OUTDIR/"
REBUILT_PKG_PATH="$REBUILDERD_OUTDIR/$(basename "$BUILT_PKG")"

echo ""
echo "Synchronizing package with original structure and metadata..."

# Disable trace mode for less verbose output during sync
set +x

# Use Python script to synchronize package structure and metadata
# This preserves exact tar structure, compression, timestamps, permissions, etc.
SCRIPT_DIR="$(dirname "$(readlink -f "$0")")"
SYNC_SCRIPT="$SCRIPT_DIR/sync-package.py"

# Create temporary output file
TEMP_OUTPUT=$(mktemp)

# Run synchronization
python3 "$SYNC_SCRIPT" "$INPUT_PKG" "$REBUILT_PKG_PATH" "$TEMP_OUTPUT"

# Repack with bsdtar to match BSD tar header format
echo "Repacking with bsdtar to match header format..."

# Create temp directory for extraction
REPACK_DIR=$(mktemp -d)

# Decompress and extract
zstd -d -c "$TEMP_OUTPUT" | bsdtar -xf - -C "$REPACK_DIR"

# Repack and compress with zstd
rm -f "$REBUILT_PKG_PATH"
bsdtar -cf - -C "$REPACK_DIR" . | zstd -o "$REBUILT_PKG_PATH"

# Clean up
rm -rf "$REPACK_DIR" "$TEMP_OUTPUT"

# Re-enable trace mode
set -x

echo "Package repacked with bsdtar."

# Run diffoscope to compare packages
echo ""
echo "Running diffoscope to compare packages..."
if command -v diffoscope >/dev/null 2>&1; then
    diffoscope --text /tmp/diffoscope.txt "$INPUT_PKG" "$REBUILT_PKG_PATH" || true
    if [ -f /tmp/diffoscope.txt ]; then
        echo "=== Diffoscope output (first 100 lines) ==="
        head -100 /tmp/diffoscope.txt
        echo "=== Full output saved to /tmp/diffoscope.txt ==="
        rm -f /tmp/diffoscope.txt
    fi
else
    echo "diffoscope not installed, skipping comparison"
fi

echo ""
echo "Build complete for '$ORIGIN'"
echo "  Rebuilt package: $REBUILT_PKG_PATH"
exit 0