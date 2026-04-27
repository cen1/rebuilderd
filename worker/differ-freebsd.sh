#!/bin/sh
set -e

# FreeBSD package diff script for rebuilderd
# Compares two FreeBSD .pkg files and determines if they are identical
#
# Arguments:
#   $1: Path to original package
#   $2: Path to rebuilt package
#
# Exit codes:
#   0: Packages are identical or content-level reproducible
#   1: Packages have actual content differences
#
# RATIONALE: upstream packages are currently not bit-for-bit reproducible.
# They don't use PKG_REPRODUCIBLE=yes and they don't set SOURCE_DATE_EPOCH. There are probably even more issues to be found..
# Instead of waiting for changes in upstream, we settle for a softer criteria of reproducibility.
#
# 1. We ignore MANIFEST and COMPACT_MANIFEST files which are missing 2 fields in rebuilt pkg.
# 2. We ignore tar headers and focus on content only.
#

ORIGINAL_PKG="$1"
REBUILT_PKG="$2"

if [ ! -f "$ORIGINAL_PKG" ]; then
    echo "Error: Original package not found: $ORIGINAL_PKG" >&2
    exit 1
fi

if [ ! -f "$REBUILT_PKG" ]; then
    echo "Error: Rebuilt package not found: $REBUILT_PKG" >&2
    exit 1
fi

# Ensure diffoscope is installed
if ! command -v diffoscope >/dev/null 2>&1; then
    echo "diffoscope not found, installing..." >&2
    pkg install -y py311-diffoscope >&2
fi

# Ensure jq is installed
if ! command -v jq >/dev/null 2>&1; then
    echo "jq not found, installing..." >&2
    pkg install -y jq >&2
fi

echo "Comparing packages..."
echo "  Original: $ORIGINAL_PKG"
echo "  Rebuilt:  $REBUILT_PKG"
echo ""

# Calculate SHA256 hashes
ORIGINAL_HASH=$(sha256 -q "$ORIGINAL_PKG")
REBUILT_HASH=$(sha256 -q "$REBUILT_PKG")

echo "Original SHA256: $ORIGINAL_HASH"
echo "Rebuilt SHA256:  $REBUILT_HASH"
echo ""

# If hashes match, packages are identical
if [ "$ORIGINAL_HASH" = "$REBUILT_HASH" ]; then
    echo "✓ SUCCESS: Packages are identical (hashes match)"
    exit 0
fi

echo "Hashes differ, checking for content-level differences..."
echo ""

# Run diffoscope with --json to determine if differences are metadata-only
# (e.g. tar headers with build timestamps differ, but file contents are identical)
DIFFOSCOPE_JSON=$(mktemp)
if ! diffoscope --json "$DIFFOSCOPE_JSON" \
    --exclude-directory-metadata=recursive \
    --exclude '*+MANIFEST' \
    --exclude '*+COMPACT_MANIFEST' \
    "$ORIGINAL_PKG" "$REBUILT_PKG" 2>&1; then
    # diffoscope returns non-zero when differences are found, which is expected
    true
fi

# Check if the only differences are tar container metadata
if jq -e '.details[]?.comments[]? | select(contains("no file-specific differences were detected"))' "$DIFFOSCOPE_JSON" >/dev/null 2>&1; then
    echo "✓ CONTENT IDENTICAL: File contents are identical, only package metadata differs"
    echo "  (This is expected due to different build timestamps in tar headers)"
    rm -f "$DIFFOSCOPE_JSON"
    exit 0
fi

# There are actual content differences
echo "✗ CONTENT DIFFERS: Actual differences found between packages"
rm -f "$DIFFOSCOPE_JSON"
exit 1
