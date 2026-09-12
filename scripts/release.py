#!/usr/bin/env python3
"""Independent product metadata and publication. No third-party dependencies."""
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET

ROOT = Path(__file__).resolve().parents[1]
FEEDS = ("update-manifest.json", "update-manifest.json.minisig", "appcast.xml")


def version(value):
    if not re.fullmatch(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)", value):
        raise ValueError(f"Expected a stable major.minor.patch version: {value}")
    return tuple(map(int, value.split(".")))


def metadata():
    data = json.loads((ROOT / "app/PruneJuice/release.json").read_text())
    version(data["version"])
    version(data["cliVersion"])
    if not re.fullmatch(r"[0-9a-f]{40}", data["cliRevision"]):
        raise ValueError("cliRevision must be a full immutable Git commit SHA")
    return data


def validate(tag):
    match = re.fullmatch(r"(cli|app)-v(.+)", tag)
    if not match:
        raise ValueError("Use cli-vX.Y.Z or app-vX.Y.Z")
    product, value = match.groups()
    version(value)
    if product == "cli":
        expected = re.search(r'^version\s*=\s*"([^"]+)"',
                             (ROOT / "Cargo.toml").read_text(), re.M)[1]
    else:
        expected = metadata()["version"]
    if value != expected:
        raise ValueError(f"{tag} does not match {product} version {expected}")
    return product, value


def gh(*args):
    return subprocess.check_output(["gh", *args], text=True).strip()


def check_helper(source):
    data = metadata()
    revision = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
    if revision != data["cliRevision"]:
        raise ValueError("Helper checkout does not match cliRevision")
    rust = (source / "crates/prune-juice-core/src/json/mod.rs").read_text()
    swift = (ROOT / "app/PruneJuice/Sources/PruneJuice/Protocol.swift").read_text()
    rust_protocol = re.search(r"PROTOCOL_VERSION: u32 = (\d+)", rust)[1]
    swift_protocol = re.search(r"supported: UInt32 = (\d+)", swift)[1]
    if rust_protocol != swift_protocol:
        raise ValueError("App does not support the pinned helper protocol")
    helper = source / "target/aarch64-apple-darwin/release/prune-juice"
    actual = subprocess.check_output([str(helper), "--version"], text=True).strip()
    if actual != f"prune-juice {data['cliVersion']}":
        raise ValueError(f"Pinned helper has unexpected version: {actual}")
    print(f"Verified pinned CLI {data['cliVersion']} and protocol {rust_protocol}")


def feed_version(product, directory):
    if product == "cli":
        path = directory / FEEDS[0]
        return json.loads(path.read_text())["version"] if path.exists() else None
    path = directory / "appcast.xml"
    if not path.exists():
        return None
    values = [node.text for node in ET.parse(path).iter()
              if node.tag == "{http://www.andymatuschak.org/xml-namespaces/sparkle}version"]
    return max(values, key=version) if values else None


def validate_appcast(directory, value):
    namespace = "http://www.andymatuschak.org/xml-namespaces/sparkle"
    items = [item for item in ET.parse(directory / "appcast.xml").findall("./channel/item")
             if item.findtext("{" + namespace + "}version") == value]
    if len(items) != 1:
        raise ValueError("Appcast must contain exactly one item for this release")
    enclosure = items[0].find("enclosure")
    if enclosure is None:
        raise ValueError("Appcast is missing its archive enclosure")
    signature = enclosure.get("{" + namespace + "}edSignature", "")
    if len(base64.b64decode(signature, validate=True)) != 64:
        raise ValueError("Appcast is missing a valid 64-byte signature")
    archive = directory / f"PruneJuice-{value}.zip"
    if int(enclosure.get("length", "0")) != archive.stat().st_size:
        raise ValueError("Appcast archive size does not match")


def publish(tag):
    product, value = validate(tag)
    # The workflow serializes publication. Never promote an older release,
    # including a manually rerun tag, over a newer product feed.
    releases = json.loads(gh("api", "--paginate", "--slurp", "repos/{owner}/{repo}/releases"))
    releases = [release for page in releases for release in page]
    channel = next((r for r in releases if r["tag_name"] == "updates"), None)
    active_channel = channel is not None and not channel["draft"]
    with tempfile.TemporaryDirectory(prefix="prune-juice-feeds-") as tmp:
        feeds = Path(tmp)
        if active_channel:
            source_tag = "updates"
        else:
            # Seed BOTH legacy feeds before taking over the latest URL.
            # A failed lookup/download is fatal, never an empty-feed fallback.
            source_tag = json.loads(gh("api", "repos/{owner}/{repo}/releases/latest"))["tag_name"]
        for asset in FEEDS:
            gh("release", "download", source_tag, "--pattern", asset, "--dir", tmp)
        previous = feed_version(product, feeds)
        existing = next((r for r in releases if r["tag_name"] == tag), None)
        if previous and (version(value) < version(previous) or
                         (version(value) == version(previous) and not existing)):
            raise ValueError(f"Refusing to replace {product} {previous} with {value}")
        selected = FEEDS[:2] if product == "cli" else FEEDS[2:]
        artifacts = sorted((ROOT / "release").glob(
            "prune-juice-*.tar.gz" if product == "cli" else "PruneJuice-*.zip"))
        if len(artifacts) != (4 if product == "cli" else 1):
            raise ValueError("Incomplete product artifacts")
        files = artifacts + [ROOT / "release" / name for name in selected]
        if feed_version(product, ROOT / "release") != value:
            raise ValueError("Generated feed version does not match the release")
        if product == "app":
            validate_appcast(ROOT / "release", value)
        if existing and not existing["draft"]:
            # Published artifacts are immutable. A retry may only republish
            # byte-identical artifacts; use a new version for rebuilt binaries.
            with tempfile.TemporaryDirectory(prefix="prune-juice-existing-") as old:
                for file in files:
                    gh("release", "download", tag, "--pattern", file.name, "--dir", old)
                    current_digest = hashlib.sha256(file.read_bytes()).digest()
                    published_digest = hashlib.sha256((Path(old) / file.name).read_bytes()).digest()
                    if current_digest != published_digest:
                        raise ValueError(f"{tag} already published with different {file.name}; bump the version")
        else:
            if not existing:
                gh("release", "create", tag, "--draft", "--verify-tag", "--latest=false",
                   "--title", f"{'CLI' if product == 'cli' else 'Mac app'} {value}", "--generate-notes")
            gh("release", "upload", tag, *map(str, files), "--clobber")
            gh("release", "edit", tag, "--draft=false", "--latest=false")
        for name in selected:
            (feeds / name).write_bytes((ROOT / "release" / name).read_bytes())
        if not channel:
            gh("release", "create", "updates", "--target", os.environ["GITHUB_SHA"],
               "--draft", "--latest=false", "--title", "Prune Juice updates",
               "--notes", "Update feeds for the independently released CLI and Mac app. Download installers from the cli-v and app-v releases.")
        # On bootstrap upload both preserved feeds. Later touch only this
        # product, so publishing a CLI can never remove the app's feed.
        names = FEEDS if not active_channel else selected
        gh("release", "upload", "updates", *(str(feeds / name) for name in names), "--clobber")
        gh("release", "edit", "updates", "--draft=false", "--latest")


def main():
    command = sys.argv[1]
    if command == "validate":
        product, value = validate(sys.argv[2])
        print(f"product={product}\nversion={value}")
    elif command == "pin":
        data = metadata()
        print(f"revision={data['cliRevision']}\nversion={data['cliVersion']}")
    elif command == "publish":
        publish(sys.argv[2])
    elif command == "check-helper":
        check_helper(Path(sys.argv[2]).resolve())
    else:
        raise ValueError(f"Unknown command: {command}")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, subprocess.CalledProcessError) as error:
        sys.exit(str(error))
