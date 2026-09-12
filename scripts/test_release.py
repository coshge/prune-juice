"""Publication tests use a fake GitHub CLI; never access GitHub."""
import base64
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("release", Path(__file__).with_name("release.py"))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


def appcast(value):
    signature = base64.b64encode(bytes(64)).decode()
    return (f'<rss xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">'
            f'<channel><item><sparkle:version>{value}</sparkle:version>'
            f'<enclosure length="3" sparkle:edSignature="{signature}" /></item></channel></rss>').encode()


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root / "app/PruneJuice").mkdir(parents=True)
        (self.root / "release").mkdir()
        (self.root / "Cargo.toml").write_text('[workspace.package]\nversion = "0.3.5"\n')
        (self.root / "app/PruneJuice/release.json").write_text(json.dumps({
            "version": "0.4.0", "cliVersion": "0.3.4", "cliRevision": "a" * 40}))
        self.patch = patch.object(release, "ROOT", self.root)
        self.patch.start()
        self.addCleanup(self.patch.stop)
        self.assets = {"v0.3.4": {
            "update-manifest.json": b'{"version":"0.3.4"}',
            "update-manifest.json.minisig": b"signature",
            "appcast.xml": appcast("0.3.4")}}
        self.drafts = set()
        self.latest = "v0.3.4"

    def fake_gh(self, *args):
        if args[0] == "api":
            if args[-1].endswith("/latest"):
                return json.dumps({"tag_name": self.latest})
            return json.dumps([[{"tag_name": tag, "draft": tag in self.drafts}
                                for tag in self.assets]])
        _, command, tag, *options = args
        if command == "download":
            name = options[options.index("--pattern") + 1]
            directory = Path(options[options.index("--dir") + 1])
            (directory / name).write_bytes(self.assets[tag][name])
        elif command == "create":
            self.assets[tag] = {}
            self.drafts.add(tag)
        elif command == "upload":
            for filename in options:
                if filename.startswith("--"):
                    continue
                path = Path(filename)
                self.assets[tag][path.name] = path.read_bytes()
        elif command == "edit":
            self.drafts.discard(tag)
            if "--latest" in options:
                self.latest = tag
        else:
            self.fail(f"Unexpected gh call: {args}")
        return ""

    def prepare(self, product, value):
        directory = self.root / "release"
        if product == "cli":
            (directory / "update-manifest.json").write_text(json.dumps({"version": value}))
            (directory / "update-manifest.json.minisig").write_bytes(b"new signature")
            for target in ("arm-mac", "intel-mac", "arm-linux", "intel-linux"):
                (directory / f"prune-juice-{value}-{target}.tar.gz").write_bytes(b"cli")
        else:
            (directory / "appcast.xml").write_bytes(appcast(value))
            (directory / f"PruneJuice-{value}.zip").write_bytes(b"app")

    def publish(self, tag):
        with patch.object(release, "gh", self.fake_gh), patch.dict(os.environ, {"GITHUB_SHA": "a" * 40}):
            release.publish(tag)

    def test_independent_versions_and_invalid_tags(self):
        self.assertEqual(release.validate("cli-v0.3.5"), ("cli", "0.3.5"))
        self.assertEqual(release.validate("app-v0.4.0"), ("app", "0.4.0"))
        for tag in ("v0.3.5", "cli-v0.4.0", "app-v0.3.5", "app-v0.4.0-beta", "app-v01.2.3"):
            with self.assertRaises(ValueError):
                release.validate(tag)

    def test_cli_bootstrap_preserves_legacy_app_then_app_preserves_cli(self):
        self.prepare("cli", "0.3.5")
        self.publish("cli-v0.3.5")
        self.assertEqual(self.latest, "updates")
        self.assertEqual(self.assets["updates"]["appcast.xml"], appcast("0.3.4"))
        cli = self.assets["updates"]["update-manifest.json"]
        self.prepare("app", "0.4.0")
        self.publish("app-v0.4.0")
        self.assertEqual(self.assets["updates"]["update-manifest.json"], cli)
        self.assertEqual(self.assets["updates"]["appcast.xml"], appcast("0.4.0"))
        self.assertNotIn("appcast.xml", self.assets["cli-v0.3.5"])
        self.assertNotIn("update-manifest.json", self.assets["app-v0.4.0"])

    def test_app_can_release_first_without_a_cli_release(self):
        self.prepare("app", "0.4.0")
        self.publish("app-v0.4.0")
        self.assertEqual(self.assets["updates"]["update-manifest.json"], self.assets["v0.3.4"]["update-manifest.json"])
        self.assertNotIn("cli-v0.4.0", self.assets)

    def test_empty_app_signature_stops_publication(self):
        self.prepare("app", "0.4.0")
        feed = self.root / "release/appcast.xml"
        feed.write_bytes(feed.read_bytes().replace(base64.b64encode(bytes(64)), b""))
        with self.assertRaisesRegex(ValueError, "signature"):
            self.publish("app-v0.4.0")
        self.assertNotIn("app-v0.4.0", self.assets)

    def test_wrong_app_archive_size_stops_publication(self):
        self.prepare("app", "0.4.0")
        (self.root / "release/PruneJuice-0.4.0.zip").write_bytes(b"wrong size")
        with self.assertRaisesRegex(ValueError, "size"):
            self.publish("app-v0.4.0")
        self.assertNotIn("app-v0.4.0", self.assets)

    def test_partial_channel_draft_is_reseeded_from_legacy_feeds(self):
        self.assets["updates"] = {}
        self.drafts.add("updates")
        self.prepare("app", "0.4.0")
        self.publish("app-v0.4.0")
        self.assertEqual(set(self.assets["updates"]), set(release.FEEDS))
        self.assertNotIn("updates", self.drafts)

    def test_missing_legacy_feed_does_not_publish(self):
        del self.assets["v0.3.4"]["appcast.xml"]
        self.prepare("cli", "0.3.5")
        with self.assertRaises(KeyError):
            self.publish("cli-v0.3.5")
        self.assertNotIn("cli-v0.3.5", self.assets)
        self.assertNotIn("updates", self.assets)

    def test_existing_version_cannot_be_reintroduced_under_a_new_tag(self):
        self.assets["v0.3.4"]["appcast.xml"] = appcast("0.4.0")
        self.prepare("app", "0.4.0")
        with self.assertRaisesRegex(ValueError, "Refusing"):
            self.publish("app-v0.4.0")

    def test_older_release_cannot_replace_feed(self):
        self.assets["updates"] = dict(self.assets["v0.3.4"])
        self.assets["updates"]["appcast.xml"] = appcast("0.5.0")
        self.prepare("app", "0.4.0")
        with self.assertRaisesRegex(ValueError, "Refusing"):
            self.publish("app-v0.4.0")
        self.assertNotIn("app-v0.4.0", self.assets)

    def test_retry_is_safe_but_changed_published_artifact_is_rejected(self):
        self.prepare("app", "0.4.0")
        self.publish("app-v0.4.0")
        self.publish("app-v0.4.0")
        (self.root / "release/PruneJuice-0.4.0.zip").write_bytes(b"bad")
        with self.assertRaisesRegex(ValueError, "already published"):
            self.publish("app-v0.4.0")
        self.assertEqual(self.assets["app-v0.4.0"]["PruneJuice-0.4.0.zip"], b"app")


if __name__ == "__main__":
    unittest.main()
