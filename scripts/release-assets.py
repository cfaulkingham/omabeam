#!/usr/bin/env python3
"""Check release versions and generate an AUR recipe from both Cast bundles."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import tarfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
ARCHES = ("x86_64", "aarch64")


def release_version(root=ROOT, tag=None):
    manifest = json.loads((root / "manifest.json").read_text())
    version = manifest["version"]
    cargo = tomllib.loads((root / "Cargo.toml").read_text())
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[a-zA-Z0-9.-]+)?", version):
        raise ValueError("Invalid release version")
    if cargo["package"]["version"] != version:
        raise ValueError("Cargo.toml and manifest.json versions must match")
    if tag is not None and tag != f"v{version}":
        raise ValueError(f"Release tag must be v{version}, got {tag}")
    return version


def generate_aur(dist, root=ROOT):
    version = release_version(root)
    # Arch's pkgver forbids hyphens. Pre-releases can still have test bundles,
    # but must not accidentally generate a stable AUR package.
    if "-" in version:
        raise ValueError("Generate the AUR package from a stable release version")
    plugin_id = json.loads((root / "manifest.json").read_text())["id"]
    recipe = (root / "packaging/aur/PKGBUILD.in").read_text().replace("@VERSION@", version)
    for arch in ARCHES:
        archive = dist / f"omabeam-{version}-{arch}-unknown-linux-gnu.tar.gz"
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        checksum = archive.with_suffix(".gz.sha256").read_text().split()
        if checksum != [digest, archive.name]:
            raise ValueError(f"Checksum mismatch for {archive.name}")
        with tarfile.open(archive) as tar:
            for name in ("omabeam", "omabeam-encoder", "omabeam-cast"):
                member = tar.getmember(f"{plugin_id}/omarchy-plugin/native/bin/{name}")
                if not member.isfile() or member.mode & 0o111 == 0:
                    raise ValueError(f"Missing executable {name} in {archive.name}")
            bundled = json.load(tar.extractfile(f"{plugin_id}/manifest.json"))
            if bundled["version"] != version or bundled["id"] != plugin_id:
                raise ValueError(f"Wrong manifest in {archive.name}")
            tar.getmember(f"{plugin_id}/licenses/cast/manifest.json")
        recipe = recipe.replace(f"@SHA256_{arch.upper()}@", digest)
    destination = dist / "aur"
    destination.mkdir(exist_ok=True)
    (destination / "PKGBUILD").write_text(recipe)
    return destination / "PKGBUILD"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--tag")
    parser.add_argument("--check-version", action="store_true")
    parser.add_argument("--dist", type=Path, default=ROOT / "dist")
    args = parser.parse_args()
    try:
        version = release_version(tag=args.tag)
        print(version if args.check_version else generate_aur(args.dist))
    except (OSError, ValueError, KeyError, tarfile.TarError) as error:
        parser.exit(1, f"release-assets: {error}\n")


if __name__ == "__main__":
    main()
