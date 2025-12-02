#!/usr/bin/env python3
"""
Synchronize a rebuilt FreeBSD package with the original package structure.

This script:
1. Reads the original package to get exact tar structure and metadata
2. Reads the rebuilt package to get new file contents
3. Creates a new package with original metadata but rebuilt contents
"""

import sys
import tarfile
import tempfile
import subprocess
import os
from pathlib import Path


def detect_compression(filepath):
    """Detect compression type from file magic bytes."""
    with open(filepath, 'rb') as f:
        header = f.read(16)

    # Zstandard magic: 0x28 0xB5 0x2F 0xFD
    if header[:4] == b'\x28\xB5\x2F\xFD':
        return 'zstd'
    # Gzip magic: 0x1F 0x8B
    elif header[:2] == b'\x1F\x8B':
        return 'gz'
    # Bzip2 magic: 'BZ'
    elif header[:2] == b'BZ':
        return 'bz2'
    # XZ magic: 0xFD 0x37 0x7A 0x58 0x5A 0x00
    elif header[:6] == b'\xFD\x37\x7A\x58\x5A\x00':
        return 'xz'
    else:
        return None


def create_pkg_header(member_name, source_path, is_manifest, orig_member):
    """
    Create a canonicalized USTAR header exactly how FreeBSD pkg writes it.

    Args:
        member_name: Full path in tar (e.g., "/usr/local/bin/foo" or "+MANIFEST")
        source_path: Path object to actual file on disk
        is_manifest: True if this is a metadata file (+MANIFEST, +COMPACT_MANIFEST, etc.)
        orig_member: Original TarInfo from the original package (for preserving timestamps)
    """
    # Create TarInfo with full path - Python's tarfile will handle USTAR name/prefix split
    tarinfo = tarfile.TarInfo(name=member_name)

    # Preserve uid/gid/owners from original package
    tarinfo.uid = orig_member.uid
    tarinfo.gid = orig_member.gid
    tarinfo.uname = orig_member.uname
    tarinfo.gname = orig_member.gname

    # Preserve mode from original package
    tarinfo.mode = orig_member.mode

    # Correct mtime rules:
    # - Manifest files (+MANIFEST, +COMPACT_MANIFEST, etc.): mtime = 0
    # - All other files: preserve original timestamp from original package
    if is_manifest:
        tarinfo.mtime = 0
    else:
        tarinfo.mtime = orig_member.mtime

    # Preserve type and linkname from original
    tarinfo.type = orig_member.type
    tarinfo.linkname = orig_member.linkname

    # For regular files, get size from actual file (content may differ for rebuilt files)
    # For other types (symlinks, dirs, etc.), preserve original size (usually 0)
    if tarinfo.isfile():
        tarinfo.size = source_path.stat().st_size
    else:
        tarinfo.size = orig_member.size

    return tarinfo


def sync_packages(original_path, rebuilt_path, output_path):
    """
    Synchronize rebuilt package with original package metadata.

    Args:
        original_path: Path to original upstream package
        rebuilt_path: Path to rebuilt package
        output_path: Path for output synchronized package
    """
    # Detect compression
    compression = detect_compression(original_path)
    print(f"Detected compression: {compression}")

    # Create temp directory for extraction
    with tempfile.TemporaryDirectory() as tmpdir:
        tmpdir = Path(tmpdir)
        orig_dir = tmpdir / "original"
        rebuilt_dir = tmpdir / "rebuilt"
        orig_dir.mkdir()
        rebuilt_dir.mkdir()

        # Decompress zstd files if needed (Python tarfile doesn't support zstd)
        if compression == 'zstd':
            print("Decompressing zstd files...")
            orig_tar_path = tmpdir / "original.tar"
            rebuilt_tar_path = tmpdir / "rebuilt.tar"

            subprocess.run(['zstd', '-d', '-c', original_path],
                         stdout=open(orig_tar_path, 'wb'), check=True)
            subprocess.run(['zstd', '-d', '-c', rebuilt_path],
                         stdout=open(rebuilt_tar_path, 'wb'), check=True)
        else:
            orig_tar_path = original_path
            rebuilt_tar_path = rebuilt_path

        # Extract both packages to get file contents
        print("Extracting packages...")
        with tarfile.open(orig_tar_path, 'r:*') as orig_tar:
            orig_members_list = orig_tar.getmembers()
            print(f"Original package has {len(orig_members_list)} members")
            # Extract with filter='data' to allow absolute paths (Python 3.12+)
            # For older Python, manually handle absolute paths
            try:
                orig_tar.extractall(orig_dir, filter='data')
            except TypeError:
                # Python < 3.12 doesn't have filter parameter
                # Extract manually, stripping leading slashes
                for member in orig_tar.getmembers():
                    if member.name.startswith('/'):
                        member.name = member.name.lstrip('/')
                    orig_tar.extract(member, orig_dir)

        with tarfile.open(rebuilt_tar_path, 'r:*') as rebuilt_tar:
            rebuilt_members_list = rebuilt_tar.getmembers()
            print(f"Rebuilt package has {len(rebuilt_members_list)} members")
            # Extract with filter='data' to allow absolute paths (Python 3.12+)
            # For older Python, manually handle absolute paths
            try:
                rebuilt_tar.extractall(rebuilt_dir, filter='data')
            except TypeError:
                # Python < 3.12 doesn't have filter parameter
                # Extract manually, stripping leading slashes
                for member in rebuilt_tar.getmembers():
                    if member.name.startswith('/'):
                        member.name = member.name.lstrip('/')
                    rebuilt_tar.extract(member, rebuilt_dir)

        # Read original package tar structure
        print("Reading original package structure...")
        orig_members = {}
        with tarfile.open(orig_tar_path, 'r:*') as orig_tar:
            for member in orig_tar.getmembers():
                orig_members[member.name] = member

        # Create new package with original structure but rebuilt contents
        print("Creating synchronized package...")

        # Determine tar mode based on compression
        if compression == 'zstd':
            # Python's tarfile doesn't support zstd directly, need to use pipe
            # Create uncompressed tar first
            temp_tar = Path(tmpdir) / "temp.tar"
            with tarfile.open(temp_tar, 'w', format=tarfile.USTAR_FORMAT) as out_tar:
                # Add members in same order as original
                for member_name, orig_member in orig_members.items():
                    # Determine source file path
                    # For manifest files, always use original
                    # For other files, use rebuilt if available, otherwise original
                    orig_file = orig_dir / member_name.lstrip('/')
                    rebuilt_file = rebuilt_dir / member_name.lstrip('/')

                    is_manifest = member_name.startswith('+') or member_name.startswith('./+')

                    if is_manifest:
                        # Always use original manifest files
                        if orig_file.exists():
                            source_file = orig_file
                        else:
                            print(f"Warning: Manifest {member_name} not found in original")
                            continue
                    elif rebuilt_file.exists():
                        source_file = rebuilt_file
                    elif orig_file.exists():
                        source_file = orig_file
                    else:
                        print(f"Warning: {member_name} not found in either package")
                        continue

                    # Create canonicalized USTAR header matching FreeBSD pkg format
                    new_member = create_pkg_header(member_name, source_file, is_manifest, orig_member)

                    # Add file to tar with appropriate data
                    if new_member.isfile():
                        with open(source_file, 'rb') as f:
                            out_tar.addfile(new_member, f)
                    else:
                        # Symlinks, directories, etc.
                        out_tar.addfile(new_member)

            # Compress with zstd
            print("Compressing with zstd...")
            subprocess.run(['zstd', '-f', '-o', output_path, temp_tar], check=True)

        else:
            # Use standard tarfile compression
            mode_map = {
                'gz': 'w:gz',
                'bz2': 'w:bz2',
                'xz': 'w:xz',
                None: 'w'
            }
            mode = mode_map.get(compression, 'w')

            with tarfile.open(output_path, mode, format=tarfile.USTAR_FORMAT) as out_tar:
                # Add members in same order as original
                for member_name, orig_member in orig_members.items():
                    # Determine source file path
                    # For manifest files, always use original
                    # For other files, use rebuilt if available, otherwise original
                    orig_file = orig_dir / member_name.lstrip('/')
                    rebuilt_file = rebuilt_dir / member_name.lstrip('/')

                    is_manifest = member_name.startswith('+') or member_name.startswith('./+')

                    if is_manifest:
                        # Always use original manifest files
                        if orig_file.exists():
                            source_file = orig_file
                        else:
                            print(f"Warning: Manifest {member_name} not found in original")
                            continue
                    elif rebuilt_file.exists():
                        source_file = rebuilt_file
                    elif orig_file.exists():
                        source_file = orig_file
                    else:
                        print(f"Warning: {member_name} not found in either package")
                        continue

                    # Create canonicalized USTAR header matching FreeBSD pkg format
                    new_member = create_pkg_header(member_name, source_file, is_manifest, orig_member)

                    # Add file to tar with appropriate data
                    if new_member.isfile():
                        with open(source_file, 'rb') as f:
                            out_tar.addfile(new_member, f)
                    else:
                        # Symlinks, directories, etc.
                        out_tar.addfile(new_member)

    print(f"Synchronized package created: {output_path}")


if __name__ == '__main__':
    if len(sys.argv) != 4:
        print("Usage: sync-package.py <original.pkg> <rebuilt.pkg> <output.pkg>")
        sys.exit(1)

    original_path = sys.argv[1]
    rebuilt_path = sys.argv[2]
    output_path = sys.argv[3]

    sync_packages(original_path, rebuilt_path, output_path)
