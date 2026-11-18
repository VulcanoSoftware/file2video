"""Verify the complete CI artifact set, then optionally publish it to GitHub."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess


PLATFORMS = {
    "linux-x86_64": "tar.gz",
    "linux-aarch64": "tar.gz",
    "macos-x86_64": "tar.gz",
    "macos-aarch64": "tar.gz",
    "windows-x86_64": "zip",
}


def verify_assets(dist):
    windows = list(dist.glob("file2video-*-windows-x86_64.zip"))
    if len(windows) != 1:
        raise ValueError("expected exactly one Windows x86_64 archive")
    version = windows[0].name.removeprefix("file2video-").removesuffix("-windows-x86_64.zip")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][0-9A-Za-z.+-]+)?", version):
        raise ValueError(f"invalid archive version: {version}")

    assets = []
    for platform, ext in PLATFORMS.items():
        archive = dist / f"file2video-{version}-{platform}.{ext}"
        checksum = archive.with_name(archive.name + ".sha256")
        with archive.open("rb") as file:
            digest = hashlib.file_digest(file, "sha256").hexdigest()
        if checksum.read_text(encoding="ascii") != f"{digest}  {archive.name}\n":
            raise ValueError(f"invalid SHA-256 checksum for {archive.name}")
        assets.extend([archive, checksum])
    if set(dist.iterdir()) != set(assets):
        raise ValueError("unexpected files in the release artifact directory")
    return version, assets


def release_plan(env, version):
    ref = env["GITHUB_REF"]
    event = env["GITHUB_EVENT_NAME"]
    if event == "push" and ref == f"refs/tags/v{version}":
        return f"v{version}", None, ["--generate-notes"]
    if ref != "refs/heads/main" or event not in ("push", "workflow_dispatch"):
        raise ValueError(f"release not permitted for {event} on {ref}")

    sha = env["GITHUB_SHA"]
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise ValueError("expected a full Git commit SHA")
    repo_url = f"{env['GITHUB_SERVER_URL']}/{env['GITHUB_REPOSITORY']}"
    run_url = f"{repo_url}/actions/runs/{env['GITHUB_RUN_ID']}"
    return f"build-{sha}", sha, [
        "--title", f"Main build {sha[:7]} (v{version})",
        "--notes", (
            f"Automated binaries for [commit {sha[:7]}]({repo_url}/commit/{sha}), "
            f"built and tested in [GitHub Actions]({run_url}).\n\n"
            "Download the archive for your OS and CPU. FFmpeg with libx264 and ffprobe "
            "must be installed separately. SHA-256 files verify the matching archives."
        ),
    ]


def ensure_build_tag(repo, tag, sha):
    lookup = subprocess.run(
        ["gh", "api", f"repos/{repo}/git/ref/tags/{tag}", "--jq", ".object.sha"],
        capture_output=True, text=True,
    )
    if lookup.returncode == 0:
        if lookup.stdout.strip() != sha:
            raise ValueError(f"existing tag {tag} does not point at the tested commit")
        return
    if not lookup.stderr.strip().endswith("(HTTP 404)"):
        raise ValueError(f"could not look up tag {tag}: {lookup.stderr.strip()}")
    # Create the tag via the refs API. Creating it implicitly via the releases
    # API can require workflow permissions that GITHUB_TOKEN cannot be granted.
    subprocess.run(
        ["gh", "api", "--method", "POST", f"repos/{repo}/git/refs",
         "-f", f"ref=refs/tags/{tag}", "-f", f"sha={sha}", "--silent"],
        check=True,
    )


def publish(repo, tag, sha, options, assets):
    if sha is not None:
        ensure_build_tag(repo, tag, sha)
    existing = subprocess.run(
        ["gh", "release", "view", tag, "--repo", repo, "--json", "assets,isDraft,url"],
        capture_output=True, text=True,
    )
    if existing.returncode == 0:
        data = json.loads(existing.stdout)
        names = {asset["name"] for asset in data["assets"] if asset["state"] == "uploaded"}
        if data["isDraft"] or not {asset.name for asset in assets} <= names:
            raise ValueError(f"existing release for {tag} is incomplete; review {data['url']}")
        print(f"Already published: {data['url']}")
        return
    if existing.stderr.strip() != "release not found":
        raise ValueError(f"could not check for an existing release: {existing.stderr.strip()}")
    if sha is not None:
        head = subprocess.check_output(
            ["gh", "api", f"repos/{repo}/git/ref/heads/main", "--jq", ".object.sha"], text=True,
        ).strip()
        # Still publish older runs, but don't deliberately mark them as Latest.
        options = [*options, "--latest" if sha == head else "--latest=false"]
    # GitHub CLI uploads files to a draft before making the release visible.
    subprocess.run(
        ["gh", "release", "create", tag, "--repo", repo, "--verify-tag", *options, *map(str, assets)],
        check=True,
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("dist", type=Path)
    parser.add_argument("--publish", action="store_true")
    args = parser.parse_args()
    try:
        version, assets = verify_assets(args.dist)
        print(f"Verified {len(PLATFORMS)} v{version} archives and their SHA-256 checksums.", flush=True)
        if args.publish:
            tag, sha, options = release_plan(os.environ, version)
            publish(os.environ["GITHUB_REPOSITORY"], tag, sha, options, assets)
    except (OSError, ValueError, KeyError, subprocess.CalledProcessError) as err:
        parser.exit(1, f"Release error: {err}\n")


if __name__ == "__main__":
    main()
