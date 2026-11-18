"""Build release archives; only used in CI (not a runtime dependency)."""
import hashlib
import os
from pathlib import Path
import subprocess
import sys
import tarfile
import zipfile


def main(platform):
    windows = platform.startswith("windows-")
    target_dir = Path("target")
    if target := os.environ.get("CARGO_BUILD_TARGET"):
        target_dir /= target
    binary = target_dir / "release" / ("file2video.exe" if windows else "file2video")
    version = subprocess.check_output([str(binary.resolve()), "--version"], text=True).split()[1]
    if os.environ.get("GITHUB_REF_TYPE") == "tag" and os.environ["GITHUB_REF_NAME"] != f"v{version}":
        raise SystemExit(f"Release tag must match Cargo.toml: expected v{version}")
    dist = Path("dist")
    dist.mkdir(exist_ok=True)
    root = f"file2video-{version}-{platform}"
    files = (binary, Path("README.md"))
    if windows:
        archive = dist / f"{root}.zip"
        with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as package:
            for file in files:
                package.write(file, f"{root}/{file.name}")
    else:
        archive = dist / f"{root}.tar.gz"
        with tarfile.open(archive, "w:gz") as package:
            for file in files:
                package.add(file, arcname=f"{root}/{file.name}")
    checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_name(archive.name + ".sha256").write_text(
        f"{checksum}  {archive.name}\n", encoding="ascii", newline="\n"
    )


if __name__ == "__main__":
    main(sys.argv[1])
