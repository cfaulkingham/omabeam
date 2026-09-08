#!/usr/bin/env python3
"""Release archives and installer behavior, using disposable homes and tools."""
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

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("package_plugin", ROOT / "scripts/package-plugin.py")
PACKAGER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PACKAGER)
PLUGIN_ID = json.loads((ROOT / "manifest.json").read_text())["id"]


def executable(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    path.chmod(0o755)
    return path


def source_copy(destination):
    destination.mkdir(parents=True)
    for name in ("manifest.json", "README.md", "LICENSE", "install.sh", "RELEASING.md", "Cargo.toml"):
        shutil.copy2(ROOT / name, destination / name)
    shutil.copytree(ROOT / "omarchy-plugin", destination / "omarchy-plugin", ignore=shutil.ignore_patterns("native", "__pycache__"))
    shutil.copytree(ROOT / "docs", destination / "docs")
    (destination / "vendor/localsend").mkdir(parents=True)
    shutil.copy2(ROOT / "vendor/localsend/LICENSE", destination / "vendor/localsend/LICENSE")
    shutil.copy2(ROOT / "vendor/localsend/UPSTREAM.md", destination / "vendor/localsend/UPSTREAM.md")


class Packaging(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="omabeam-package-")
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.source = self.base / "source with spaces"
        source_copy(self.source)
        self.binary = self.base / "test-binary"
        header = bytearray(64)
        header[:6] = b"\x7fELF\x02\x01"
        header[16:18] = (3).to_bytes(2, "little")
        header[18:20] = (62).to_bytes(2, "little")
        self.binary.write_bytes(header)
        self.binary.chmod(0o755)
        self.licenses = self.base / "THIRDPARTY.yml"
        self.licenses.write_text("test-only license fixture\n")

    def package(self, target="x86_64-unknown-linux-gnu"):
        return PACKAGER.package(self.binary, target, self.licenses, self.base / "dist", root=self.source)

    def test_archive_layout_modes_checksum_and_reproducibility(self):
        (self.source / "omarchy-plugin/native/bin").mkdir(parents=True)
        (self.source / "omarchy-plugin/native/bin/old-binary").write_text("must not ship")
        archive = self.package()
        original = archive.read_bytes()
        self.assertEqual(original, self.package().read_bytes())
        self.assertTrue(archive.with_suffix(".gz.sha256").read_text().startswith(hashlib.sha256(original).hexdigest()))
        with tarfile.open(archive) as tar:
            manifest = json.load(tar.extractfile(f"{PLUGIN_ID}/manifest.json"))
            self.assertIn(f"{PLUGIN_ID}/{manifest['entryPoints']['barWidget']}", tar.getnames())
            self.assertEqual(tar.getmember(f"{PLUGIN_ID}/omarchy-plugin/native/bin/omabeam").mode, 0o755)
            self.assertEqual(tar.getmember(f"{PLUGIN_ID}/omarchy-plugin/omabeam").mode, 0o755)
            self.assertTrue(all(member.isfile() for member in tar.getmembers()))
            self.assertIn(f"{PLUGIN_ID}/docs/DEVELOPMENT.md", tar.getnames())
            self.assertIn(f"{PLUGIN_ID}/vendor/localsend/UPSTREAM.md", tar.getnames())
            self.assertFalse(any("old-binary" in name or "Cargo.toml" in name for name in tar.getnames()))

    def test_rejects_wrong_binary_symlink_and_unsafe_entry(self):
        with self.assertRaisesRegex(ValueError, "ELF"):
            self.package("aarch64-unknown-linux-gnu")
        panel = self.source / "omarchy-plugin/Panel.qml"
        panel.unlink()
        panel.symlink_to(ROOT / "omarchy-plugin/Panel.qml")
        with self.assertRaisesRegex(ValueError, "symlinks"):
            self.package()
        manifest_path = self.source / "manifest.json"
        manifest = json.loads(manifest_path.read_text())
        manifest["entryPoints"]["barWidget"] = "../private.qml"
        manifest_path.write_text(json.dumps(manifest))
        with self.assertRaisesRegex(ValueError, "entry point"):
            self.package()

    def test_launcher_requires_plugin_binary_and_preserves_literal_arguments(self):
        home, tools = self.base / "home", self.base / "tools"
        tools.mkdir()
        env = {**os.environ, "HOME": str(home), "PATH": f"{tools}:/usr/bin:/bin"}
        launcher = self.source / "omarchy-plugin/omabeam"
        backend = self.source / "omarchy-plugin/native/bin/omabeam"
        executable(backend, '#!/bin/sh\nprintf "%s\\n" "plugin-binary" "$@"\n')
        result = subprocess.run([launcher, "--send-link", "a b; $(touch nope)"], env=env, capture_output=True, text=True, check=True)
        self.assertEqual(result.stdout.splitlines(), ["plugin-binary", "--send-link", "a b; $(touch nope)"])
        backend.unlink()
        # Missing plugin binaries must not launch another version from PATH.
        executable(tools / "omabeam", '#!/bin/sh\necho unexpected-system-binary\n')
        result = subprocess.run([launcher, "--status"], env=env, capture_output=True, text=True, timeout=3)
        self.assertEqual(result.returncode, 127)
        self.assertEqual(result.stdout, "")
        self.assertIn("--backend-only", result.stderr)

    def test_installer_build_bundle_upgrade_and_git_checkout(self):
        home, tools = self.base / "home", self.base / "tools"
        tools.mkdir()
        config = home / "config with spaces"
        hypr = config / "hypr"
        hypr.mkdir(parents=True)
        (hypr / "hyprland.lua").write_text("-- user config\n")
        (hypr / "bindings.lua").write_text('-- user binding\n-- omabeam (install.sh)\no.bind("SUPER + SHIFT + T", "OmaBeam", { launch = "/old/bin/omabeam" })\n')
        native = f'#!{sys.executable}\nimport sys\nif sys.argv[1:] == ["--hypr", "version"]: sys.exit(1)\nprint("native app")\n'
        executable(tools / "uname", "#!/bin/sh\necho Linux\n")
        executable(tools / "wl-copy", "#!/bin/sh\nexit 0\n")
        executable(tools / "omarchy-shell", "#!/bin/sh\nexit 0\n")
        executable(tools / "omarchy", f'''#!{sys.executable}
import json, sys
from pathlib import Path
if sys.argv[1:3] == ["plugin", "validate"]:
    root = Path(sys.argv[3])
    manifest = json.loads((root / "manifest.json").read_text())
    assert (root / manifest["entryPoints"]["barWidget"]).is_file()
''')
        executable(tools / "cargo", f'''#!{sys.executable}
import sys
from pathlib import Path
assert "--locked" in sys.argv
root = Path(sys.argv[sys.argv.index("--root") + 1])
path = root / "bin/omabeam"
path.parent.mkdir(parents=True, exist_ok=True)
path.write_text({native!r})
path.chmod(0o755)
''')
        env = {**os.environ, "HOME": str(home), "XDG_CONFIG_HOME": str(config), "XDG_CACHE_HOME": str(home / "cache"), "PATH": f"{tools}:{os.environ['PATH']}"}
        def install(path, *args, success=True):
            result = subprocess.run(["bash", str(path / "install.sh"), *args], env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
            return result
        install(self.source, "--backend-only")
        self.assertFalse((config / "omarchy").exists())
        install(self.source)
        installed = config / "omarchy/plugins" / PLUGIN_ID
        self.assertTrue((installed / "omarchy-plugin/native/bin/omabeam").is_file())
        self.assertTrue((installed / "RELEASING.md").is_file())
        self.assertIn(str(installed / "omarchy-plugin/omabeam"), (hypr / "bindings.lua").read_text())
        before = [(hypr / name).read_text() for name in ("hyprland.lua", "bindings.lua")]
        # Copied runtime-only installs can rerun without Cargo/source present.
        (tools / "cargo").unlink()
        install(installed)
        self.assertEqual(before, [(hypr / name).read_text() for name in ("hyprland.lua", "bindings.lua")])
        (installed / ".git").mkdir()
        (installed / ".git/sentinel").write_text("keep")
        self.assertIn("Git-managed", install(self.source, success=False).stderr)
        install(installed)
        self.assertEqual((installed / ".git/sentinel").read_text(), "keep")


if __name__ == "__main__":
    unittest.main(verbosity=2)
