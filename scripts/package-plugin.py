#!/usr/bin/env python3
"""Package a built Linux app and the Omarchy UI, without running plugin code."""
import argparse
import gzip
import hashlib
import json
import os
from pathlib import Path
import re
import tarfile
import tempfile

ROOT = Path(__file__).resolve().parents[1]
MACHINES = {"x86_64-unknown-linux-gnu": 62, "aarch64-unknown-linux-gnu": 183}


def validate_binary(binary, target):
    if binary.is_symlink() or not binary.is_file():
        raise ValueError("Native binaries must be regular files, not symlinks")
    with binary.open("rb") as source:
        header = source.read(64)
    if (len(header) < 64 or header[:6] != b"\x7fELF\x02\x01"
            or int.from_bytes(header[16:18], "little") not in (2, 3)
            or int.from_bytes(header[18:20], "little") != MACHINES[target]):
        raise ValueError(f"The binary must be a 64-bit Linux ELF executable for {target}")
    if not os.access(binary, os.X_OK):
        raise ValueError("The native binary must be executable")


def package(binary, target, licenses, output, root=ROOT, encoder_helper=None):
    manifest = json.loads((root / "manifest.json").read_text())
    plugin_id, version = manifest["id"], manifest["version"]
    if not re.fullmatch(r"[a-z0-9]+(?:[.-][a-z0-9-]+)+", plugin_id) or plugin_id.startswith("omarchy."):
        raise ValueError("Use a permanent third-party namespaced plugin ID")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[a-zA-Z0-9.-]+)?", version):
        raise ValueError("The release version must be a safe semantic version")
    if manifest.get("schemaVersion") != 1 or manifest.get("kinds") != ["bar-widget"]:
        raise ValueError("Expected the OmaBeam bar-widget manifest")

    validate_binary(binary, target)
    if encoder_helper is not None:
        validate_binary(encoder_helper, target)
    if not licenses.read_text().strip():
        raise ValueError("Provide the third-party license bundle")

    # An allowlist prevents build caches, local settings, and old binaries from
    # leaking into releases. The binary and license bundle are explicit inputs.
    files = {name: root / name for name in ("manifest.json", "README.md", "LICENSE", "install.sh", "RELEASING.md")}
    for path in sorted((root / "docs").glob("*.md")):
        files[str(path.relative_to(root))] = path
    for path in sorted((root / "omarchy-plugin").iterdir()):
        if path.suffix in (".qml", ".js") or path.name in ("omabeam", "firewall.py"):
            files[str(path.relative_to(root))] = path
    files["omarchy-plugin/native/bin/omabeam"] = binary
    if encoder_helper is not None:
        files["omarchy-plugin/native/bin/omabeam-encoder"] = encoder_helper
    files["licenses/THIRDPARTY.yml"] = licenses
    files["licenses/localsend-LICENSE"] = root / "vendor/localsend/LICENSE"
    files["vendor/localsend/UPSTREAM.md"] = root / "vendor/localsend/UPSTREAM.md"
    entry = manifest["entryPoints"]["barWidget"]
    if entry not in files or not entry.endswith(".qml"):
        raise ValueError("The barWidget entry point must be a bundled relative QML path")
    for path in files.values():
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"Release files must be regular files, not symlinks: {path}")

    output.mkdir(parents=True, exist_ok=True)
    archive = output / f"omabeam-{version}-{target}.tar.gz"
    # Stable ordering, timestamps, ownership, and modes make identical inputs
    # produce the same archive and checksum, regardless of the build directory.
    with tempfile.NamedTemporaryFile(dir=output, delete=False) as temporary:
        temporary_path = Path(temporary.name)
        try:
            with gzip.GzipFile(filename="", fileobj=temporary, mode="wb", mtime=0) as compressed:
                with tarfile.open(fileobj=compressed, mode="w") as tar:
                    for name, path in sorted(files.items()):
                        info = tarfile.TarInfo(f"{plugin_id}/{name}")
                        info.size = path.stat().st_size
                        info.mode = 0o755 if name in ("install.sh", "omarchy-plugin/omabeam", "omarchy-plugin/native/bin/omabeam", "omarchy-plugin/native/bin/omabeam-encoder") else 0o644
                        with path.open("rb") as source:
                            tar.addfile(info, source)
            temporary.flush()
            temporary_path.replace(archive)
        finally:
            temporary_path.unlink(missing_ok=True)
    with archive.open("rb") as source:
        checksum = hashlib.file_digest(source, "sha256").hexdigest()
    archive.with_suffix(archive.suffix + ".sha256").write_text(f"{checksum}  {archive.name}\n")
    return archive


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--encoder-helper", required=True, type=Path)
    parser.add_argument("--target", required=True, choices=MACHINES)
    parser.add_argument("--licenses", required=True, type=Path, help="Reviewed cargo-bundle-licenses YAML")
    parser.add_argument("--output", type=Path, default=ROOT / "dist")
    args = parser.parse_args()
    try:
        print(package(args.binary, args.target, args.licenses, args.output, encoder_helper=args.encoder_helper))
    except (OSError, ValueError, KeyError) as error:
        parser.exit(1, f"package-plugin: {error}\n")


if __name__ == "__main__":
    main()
