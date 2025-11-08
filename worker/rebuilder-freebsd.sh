#!/bin/sh
set -xe

# rebuilder-freebsd.sh: A script to rebuild FreeBSD packages for rebuilderd.
# This script is invoked by the rebuilderd-worker.

# Arguments:
#   $1: Path to the .txz package to rebuild.

# Environment variables:
#   REBUILDERD_OUTDIR: Directory to place the rebuilt package in.

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

ORIGIN=$(grep -o '"origin":"[^"]*"' "$MANIFEST_PATH" | cut -d ':' -f 2 | tr -d '"')
PKG_NAME=$(grep -o '"name":"[^"]*"' "$MANIFEST_PATH" | cut -d ':' -f 2 | tr -d '"')
PKG_VERSION=$(grep -o '"version":"[^"]*"' "$MANIFEST_PATH" | cut -d ':' -f 2 | tr -d '"')

# Extract FreeBSD architecture info (format: FreeBSD:14:amd64)
PKG_ARCH_FULL=$(pkg info -F "$INPUT_PKG" | grep '^Architecture' | awk '{print $3}')
FREEBSD_MAJOR=$(echo "$PKG_ARCH_FULL" | cut -d ':' -f 2)
PKG_ARCH=$(echo "$PKG_ARCH_FULL" | cut -d ':' -f 3)

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
JAIL="rebuilderd-${WORKER_NAME}-${FREEBSD_MAJOR}-${PKG_ARCH}"
# Use separate ports tree per worker to avoid git checkout conflicts
PORTS_TREE="worker${WORKER_NAME}"

echo "Using jail: $JAIL (FreeBSD $FREEBSD_RELEASE)"

# Check if jail exists, create if not
if ! poudriere jail -l -q | grep -q "^${JAIL} "; then
  echo "Jail '$JAIL' not found, creating..."
  poudriere jail -c -j "$JAIL" -v "$FREEBSD_RELEASE" -a "$PKG_ARCH"
  if [ $? -ne 0 ]; then
    echo "Error: Failed to create jail '$JAIL'." >&2
    exit 1
  fi
  echo "Jail '$JAIL' created successfully."
else
  echo "Jail '$JAIL' already exists."
fi

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
if ! poudriere ports -l -q | grep -q "^${PORTS_TREE} "; then
  echo "Ports tree '$PORTS_TREE' not found, creating with full git history..."
  poudriere ports -c -p "$PORTS_TREE" -m git+https -D
  if [ $? -ne 0 ]; then
    echo "Error: Failed to create ports tree '$PORTS_TREE'." >&2
    exit 1
  fi
  echo "Ports tree '$PORTS_TREE' created successfully."
else
  echo "Ports tree '$PORTS_TREE' already exists."
fi

# Create necessary directories
mkdir -p /usr/ports/distfiles
mkdir -p /tmp/rebuilderd-output

# Extract the git commit hash from the package annotations
PORTS_GIT_HASH=$(pkg info -F "$INPUT_PKG" | grep 'ports_top_git_hash' | awk '{print $2}')

if [ -z "$PORTS_GIT_HASH" ]; then
  echo "Error: Could not extract ports_top_git_hash from '$INPUT_PKG'." >&2
  exit 1
fi

echo "Package was built with ports tree commit: $PORTS_GIT_HASH"

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

# Check if the jail is running (can stay up when stopping the service)
if poudriere jail -l | grep -q "^${JAIL}.*|.*running"; then
  echo "Jail ${JAIL} is running — stopping it first..."
  poudriere jail -k -j "${JAIL}"
fi

# The following command will build the port and its dependencies.
# The official build flags should be configured in poudriere.conf, for example:
#   ALLOW_MAKE_JOBS=yes
#   PREPARE_PARALLEL_JOBS=4
#   MAKE_JOBS_NUMBER=4
# Also, respect flags like WITH_DOCS=off, etc. by configuring them in make.conf.
poudriere bulk -j "$JAIL" -p "$PORTS_TREE" "$ORIGIN"

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

# 5. Compare the packages (optional)
# If diffoscope is installed, generate a comparison report.
if command -v diffoscope >/dev/null 2>&1; then
  # Compare the original and rebuilt packages.
  diffoscope "$INPUT_PKG" "$REBUILT_PKG_PATH" > "$REBUILDERD_OUTDIR/diffoscope.html" || true
fi

echo "Successfully rebuilt '$ORIGIN' as '$REBUILT_PKG_PATH'."