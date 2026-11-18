import hashlib
import json
from pathlib import Path
from subprocess import CompletedProcess
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import patch

from scripts.publish_release import PLATFORMS, ensure_build_tag, publish, release_plan, verify_assets


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        scratch = TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.dist = Path(scratch.name)
        for platform, ext in PLATFORMS.items():
            archive = self.dist / f"file2video-0.1.0-{platform}.{ext}"
            data = platform.encode()
            archive.write_bytes(data)
            checksum = archive.with_name(archive.name + ".sha256")
            checksum.write_text(f"{hashlib.sha256(data).hexdigest()}  {archive.name}\n", encoding="ascii")

    def test_complete_asset_set(self):
        version, assets = verify_assets(self.dist)
        self.assertEqual(version, "0.1.0")
        self.assertEqual(len(assets), 10)

    def test_missing_architecture(self):
        (self.dist / "file2video-0.1.0-macos-x86_64.tar.gz").unlink()
        with self.assertRaises(FileNotFoundError):
            verify_assets(self.dist)

    def test_invalid_checksum(self):
        (self.dist / "file2video-0.1.0-linux-aarch64.tar.gz").write_bytes(b"corrupted")
        with self.assertRaisesRegex(ValueError, "invalid SHA-256 checksum"):
            verify_assets(self.dist)

    def test_unexpected_asset(self):
        (self.dist / "unrelated.zip").touch()
        with self.assertRaisesRegex(ValueError, "unexpected files"):
            verify_assets(self.dist)

    def test_main_release_uses_exact_commit(self):
        env = {
            "GITHUB_REF": "refs/heads/main", "GITHUB_EVENT_NAME": "push",
            "GITHUB_SHA": "0a" * 20, "GITHUB_REPOSITORY": "owner/repo",
            "GITHUB_SERVER_URL": "https://github.com", "GITHUB_RUN_ID": "123",
        }
        for event in ("push", "workflow_dispatch"):
            env["GITHUB_EVENT_NAME"] = event
            tag, sha, options = release_plan(env, "0.1.0")
            self.assertEqual(sha, env["GITHUB_SHA"])
            self.assertEqual(tag, "build-" + sha)
            self.assertNotIn("--target", options)

    def test_only_main_and_matching_tags_can_publish(self):
        with self.assertRaises(ValueError):
            release_plan({"GITHUB_REF": "refs/pull/1/merge", "GITHUB_EVENT_NAME": "pull_request"}, "0.1.0")
        with self.assertRaises(ValueError):
            release_plan({"GITHUB_REF": "refs/tags/v0.2.0", "GITHUB_EVENT_NAME": "push"}, "0.1.0")
        tag, sha, options = release_plan({"GITHUB_REF": "refs/tags/v0.1.0", "GITHUB_EVENT_NAME": "push"}, "0.1.0")
        self.assertEqual(tag, "v0.1.0")
        self.assertIsNone(sha)
        self.assertIn("--generate-notes", options)

    @patch("scripts.publish_release.subprocess.check_output")
    @patch("scripts.publish_release.subprocess.run")
    def test_new_release_creates_and_verifies_tag_before_upload(self, run, check_output):
        _, assets = verify_assets(self.dist)
        sha = "0a" * 20
        tag = "build-" + sha
        for head, latest in ((sha, "--latest"), ("0b" * 20, "--latest=false")):
            with self.subTest(head=head):
                run.reset_mock()
                run.side_effect = [
                    CompletedProcess([], 1, "", "gh: Not Found (HTTP 404)\n"),
                    CompletedProcess([], 0),
                    CompletedProcess([], 1, "", "release not found\n"),
                    CompletedProcess([], 0),
                ]
                check_output.return_value = head + "\n"
                publish("owner/repo", tag, sha, ["--notes", "Test release"], assets)
                create_ref = run.call_args_list[1].args[0]
                self.assertIn("repos/owner/repo/git/refs", create_ref)
                self.assertIn(f"ref=refs/tags/{tag}", create_ref)
                self.assertIn(f"sha={sha}", create_ref)
                create_release = run.call_args_list[3].args[0]
                self.assertEqual(create_release[:4], ["gh", "release", "create", tag])
                self.assertIn("--verify-tag", create_release)
                self.assertIn(latest, create_release)
                self.assertEqual(create_release[-10:], [str(asset) for asset in assets])

    @patch("scripts.publish_release.subprocess.run")
    def test_do_not_move_existing_tag_at_wrong_commit(self, run):
        run.return_value = CompletedProcess([], 0, "0b" * 20 + "\n", "")
        with self.assertRaisesRegex(ValueError, "does not point at the tested commit"):
            ensure_build_tag("owner/repo", "build-" + "0a" * 20, "0a" * 20)
        self.assertEqual(run.call_count, 1)

    @patch("scripts.publish_release.print")
    @patch("scripts.publish_release.subprocess.run")
    def test_do_not_replace_existing_release_on_retry(self, run, _):
        _, assets = verify_assets(self.dist)
        sha = "0a" * 20
        run.side_effect = [
            CompletedProcess([], 0, sha + "\n", ""),
            CompletedProcess([], 0, json.dumps({
                "assets": [{"name": asset.name, "state": "uploaded"} for asset in assets],
                "isDraft": False,
                "url": "https://github.com/owner/repo/releases",
            }), ""),
        ]
        publish("owner/repo", "build-" + sha, sha, [], assets)
        self.assertEqual(run.call_count, 2)
