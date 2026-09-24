#!/usr/bin/env python3
"""Release version, dual-architecture AUR inputs and system-package integration."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest

from packaging import ROOT, PACKAGER, PLUGIN_ID, executable, install_env, source_copy

SPEC = importlib.util.spec_from_file_location("release_assets", ROOT / "scripts/release-assets.py")
RELEASE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(RELEASE)


class Release(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="omabeam-release-")
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.source = self.base / "source"
        source_copy(self.source)
        shutil.copytree(ROOT / "packaging", self.source / "packaging")
        self.dist = self.base / "dist"

    def bundle(self, arch, with_cast=True):
        binary = self.base / arch
        header = bytearray(64)
        header[:6] = b"\x7fELF\x02\x01"
        header[16:18] = (3).to_bytes(2, "little")
        header[18:20] = (62 if arch == "x86_64" else 183).to_bytes(2, "little")
        binary.write_bytes(header)
        binary.chmod(0o755)
        licenses = self.base / "THIRDPARTY.yml"
        licenses.write_text("test-only license fixture\n")
        notices = self.base / "notices"
        notices.mkdir(exist_ok=True)
        (notices / "LICENSE").write_text("test-only Cast notice\n")
        (notices / "manifest.json").write_text(json.dumps({
            "upstream": {"test": True},
            "files": [{"path": "LICENSE", "sha256": hashlib.sha256((notices / "LICENSE").read_bytes()).hexdigest()}],
            "binary": {"protocol": 1, "sha256": hashlib.sha256(header).hexdigest()},
        }))
        return PACKAGER.package(binary, f"{arch}-unknown-linux-gnu", licenses, self.dist,
                                root=self.source, encoder_helper=binary,
                                cast_helper=binary if with_cast else None,
                                cast_licenses=notices if with_cast else None)

    def test_version_and_tag_must_match(self):
        version = RELEASE.release_version(self.source)
        self.assertEqual(RELEASE.release_version(self.source, f"v{version}"), version)
        for tag in (version, "v9.9.9", "v0.1.0;false"):
            with self.assertRaisesRegex(ValueError, "Release tag"):
                RELEASE.release_version(self.source, tag)
        cargo = self.source / "Cargo.toml"
        cargo.write_text(cargo.read_text().replace(f'version = "{version}"', 'version = "9.9.9"', 1))
        with self.assertRaisesRegex(ValueError, "versions must match"):
            RELEASE.release_version(self.source)

    def test_aur_requires_both_architectures_checksums_and_cast(self):
        x86 = self.bundle("x86_64")
        with self.assertRaises(FileNotFoundError):
            RELEASE.generate_aur(self.dist, self.source)
        arm = self.bundle("aarch64")
        recipe = RELEASE.generate_aur(self.dist, self.source).read_text()
        self.assertNotIn("@", recipe)
        for archive in (x86, arm):
            self.assertIn(hashlib.sha256(archive.read_bytes()).hexdigest(), recipe)
        arm.write_bytes(arm.read_bytes() + b"tampered")
        with self.assertRaisesRegex(ValueError, "Checksum mismatch"):
            RELEASE.generate_aur(self.dist, self.source)
        self.bundle("aarch64", with_cast=False)
        with self.assertRaises(KeyError):
            RELEASE.generate_aur(self.dist, self.source)

    @unittest.skipUnless(sys.platform == "linux", "AUR staging uses GNU install")
    def test_aur_package_layout(self):
        archive = self.bundle("x86_64")
        self.bundle("aarch64")
        recipe = RELEASE.generate_aur(self.dist, self.source)
        staging = self.base / "src"
        staging.mkdir()
        with tarfile.open(archive) as tar:
            tar.extractall(staging, filter="data")
        pkg = self.base / "pkg"
        subprocess.run(["bash", "-c", 'source "$1"; package', "bash", str(recipe)],
                       env={**os.environ, "srcdir": str(staging), "pkgdir": str(pkg)}, check=True)
        self.assertEqual((pkg / "usr/bin/omabeam").resolve(), pkg / "usr/lib/omabeam/omabeam")
        for name in ("omabeam", "omabeam-encoder", "omabeam-cast"):
            self.assertTrue(os.access(pkg / "usr/lib/omabeam" / name, os.X_OK))
        self.assertFalse((pkg / "usr/share/omabeam/plugin/omarchy-plugin/native").exists())
        self.assertTrue((pkg / "usr/share/omabeam/plugin/vendor/localsend/UPSTREAM.md").is_file())
        self.assertTrue((pkg / "usr/share/licenses/omabeam-bin/cast/manifest.json").is_file())

    def test_system_install_does_not_copy_binaries_and_launcher_preserves_arguments(self):
        env, _ = install_env(self.base)
        system = self.base / "system-bin"
        # Repoint only the test copy of the fixed system path; no host install.
        installer = self.source / "install.sh"
        installer.write_text(installer.read_text().replace("/usr/lib/omabeam", str(system)))
        launcher = self.source / "omarchy-plugin/omabeam"
        launcher.write_text(launcher.read_text().replace("/usr/lib/omabeam", str(system)))
        (self.source / "Cargo.toml").unlink()
        executable(system / "omabeam", f'#!{sys.executable}\nimport json, sys\nprint(json.dumps(sys.argv[1:]))\n')
        executable(system / "omabeam-cast", '#!/bin/sh\necho "omabeam-cast protocol=1 test"\n')
        result = subprocess.run(["bash", str(installer), "--with-cast"], env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        installed = Path(env["XDG_CONFIG_HOME"]) / "omarchy/plugins" / PLUGIN_ID
        self.assertFalse((installed / "omarchy-plugin/native").exists())
        arguments = ["--demo", "literal $(no-command) with spaces"]
        result = subprocess.run(["bash", str(installed / "omarchy-plugin/omabeam"), *arguments],
                                env=env, capture_output=True, text=True, check=True)
        self.assertEqual(json.loads(result.stdout), arguments)
        executable(installed / "omarchy-plugin/native/bin/omabeam", '#!/bin/sh\necho local-bundle\n')
        result = subprocess.run(["bash", str(installed / "omarchy-plugin/omabeam")],
                                env=env, capture_output=True, text=True, check=True)
        self.assertEqual(result.stdout.strip(), "local-bundle")


if __name__ == "__main__":
    unittest.main(verbosity=2)
