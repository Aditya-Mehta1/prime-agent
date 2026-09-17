#!/usr/bin/env python3
"""Verify an assembled release tarball (host build) end to end.

The local mirror of the CI build-job gates (docs/installer-ci-design.md §9):
the archive must contain exactly the designed payload at the tarball root,
checksums must match SHA256SUMS and manifest.json, and the staged binary must
report the release version from a scratch cwd with `PI_PACKAGE_DIR` unset
(the shipped artifact never depends on it).

Usage:
    python3 scripts/release/verify_release.py \
        --dist-dir <dir> --version <x.y.z> --target <triple>
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
from pathlib import Path

# Must mirror STAGED_ENTRIES in assemble_artifacts.py and §5 of the design doc.
EXPECTED_TOP_LEVEL = {
    "prime-agent",
    "prime-agent-runtime",
    "skills",
    "docs",
    "LICENSE",
    "README.md",
}


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def fail(message: str) -> None:
    print(f"error: {message}", file=sys.stderr)
    sys.exit(1)


def top_level_members(archive: tarfile.TarFile) -> set:
    return {
        member.name.split("/", maxsplit=1)[0]
        for member in archive.getmembers()
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--dist-dir", required=True, type=Path)
    parser.add_argument("--version", required=True)
    parser.add_argument("--target", required=True)
    args = parser.parse_args()

    archive_name = f"prime-agent-{args.version}-{args.target}.tar.gz"
    archive = args.dist_dir / archive_name
    if not archive.is_file():
        fail(f"archive {archive} not found; run assemble_artifacts.py first")

    # 1. Deterministic tarball shape: exact top-level payload, no link entries.
    with tarfile.open(archive) as tar:
        members = tar.getmembers()
        if top_level_members(tar) != EXPECTED_TOP_LEVEL:
            fail(
                f"tarball top-level entries {sorted(top_level_members(tar))} "
                f"!= designed payload {sorted(EXPECTED_TOP_LEVEL)}"
            )
        for member in members:
            if member.issym() or member.islnk():
                fail(f"tarball contains a link entry {member.name!r}")
            if member.uid != 0 or member.gid != 0 or member.mtime != 0:
                fail(f"tarball entry {member.name!r} is not deterministic "
                     f"(uid={member.uid} gid={member.gid} mtime={member.mtime})")

    # 2. Checksums agree with SHA256SUMS and manifest.json.
    archive_sha = sha256_file(archive)
    sums_path = args.dist_dir / "SHA256SUMS"
    sums = {
        Path(name.strip()).name: digest.strip()
        for digest, name in
        (line.split(None, 1) for line in sums_path.read_text().splitlines() if line.strip())
    }
    if sums.get(archive_name) != archive_sha:
        fail(f"SHA256SUMS mismatch for {archive_name}")
    manifest = json.loads((args.dist_dir / "manifest.json").read_text())
    entries = {b["file"]: b for b in manifest["binaries"]}
    if archive_name not in entries:
        fail(f"manifest.json has no entry for {archive_name}")
    if entries[archive_name]["sha256"] != archive_sha:
        fail(f"manifest.json sha256 mismatch for {archive_name}")

    # 3. The staged binary reports the release version from a scratch cwd with
    #    PI_PACKAGE_DIR unset: shipped artifacts never depend on it.
    scratch = Path(tempfile.mkdtemp(prefix="prime-agent-verify-"))
    try:
        # Manual extraction: the deterministic-shape checks above already
        # rejected link entries, and staying on explicit member writes keeps
        # the gate working on any Python >= 3.8 (no `filter=` kwarg).
        with tarfile.open(archive) as tar:
            for member in tar.getmembers():
                target = scratch / member.name
                if member.isdir():
                    target.mkdir(parents=True, exist_ok=True)
                else:
                    target.parent.mkdir(parents=True, exist_ok=True)
                    source = tar.extractfile(member)
                    assert source is not None  # plain-file member, links rejected above
                    with source, open(target, "wb") as sink:
                        shutil.copyfileobj(source, sink)
                    os.chmod(target, member.mode)
        binary = scratch / "prime-agent"
        if not os.access(binary, os.X_OK):
            fail("staged prime-agent is not executable")
        if entries[archive_name]["executableSha256"] != sha256_file(binary):
            fail("executableSha256 in manifest.json does not match the staged binary")
        env = {k: v for k, v in os.environ.items() if k != "PI_PACKAGE_DIR"}
        run = subprocess.run(
            [str(binary), "--version"],
            capture_output=True, text=True, env=env, cwd=scratch, check=False,
        )
        if run.returncode != 0:
            fail(f"staged prime-agent --version failed: {run.stderr.strip()}")
        version_out = run.stdout.strip()
        if version_out != args.version:
            fail(f"staged prime-agent reports {version_out!r}, expected {args.version!r}")
    finally:
        shutil.rmtree(scratch, ignore_errors=True)

    print(f"verified {archive_name}: payload, checksums, manifest, livecheck all OK")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
