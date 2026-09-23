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
from firewall import mock_firewall

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
    (destination / "crates/omabeam-encoder").mkdir(parents=True)
    shutil.copy2(ROOT / "crates/omabeam-encoder/Cargo.toml", destination / "crates/omabeam-encoder/Cargo.toml")
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
        return PACKAGER.package(self.binary, target, self.licenses, self.base / "dist", root=self.source, encoder_helper=self.binary)

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
            self.assertEqual(tar.getmember(f"{PLUGIN_ID}/omarchy-plugin/native/bin/omabeam-encoder").mode, 0o755)
            self.assertEqual(tar.getmember(f"{PLUGIN_ID}/omarchy-plugin/omabeam").mode, 0o755)
            self.assertTrue(all(member.isfile() for member in tar.getmembers()))
            self.assertIn(f"{PLUGIN_ID}/docs/DEVELOPMENT.md", tar.getnames())
            self.assertIn(f"{PLUGIN_ID}/omarchy-plugin/firewall.py", tar.getnames())
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

    def test_rejects_an_invalid_or_non_executable_encoder_helper(self):
        helper = self.base / "helper"
        helper.write_bytes(b"wrong architecture")
        helper.chmod(0o755)
        def package():
            return PACKAGER.package(self.binary, "x86_64-unknown-linux-gnu", self.licenses,
                self.base / "dist", root=self.source, encoder_helper=helper)
        with self.assertRaisesRegex(ValueError, "ELF"):
            package()
        helper.write_bytes(self.binary.read_bytes())
        helper.chmod(0o644)
        with self.assertRaisesRegex(ValueError, "executable"):
            package()

    def test_cast_helper_requires_matching_binary_and_notice_inventory(self):
        notices = self.base / "cast-notices"
        notices.mkdir()
        license_file = notices / "LICENSE"
        license_file.write_text("test-only native license\n")
        manifest = {"upstream":{"fixture":"test"}, "binary":{"protocol":1,
            "sha256":hashlib.sha256(self.binary.read_bytes()).hexdigest()}, "files":[{
            "path":"LICENSE", "sha256":hashlib.sha256(license_file.read_bytes()).hexdigest()}]}
        (notices / "manifest.json").write_text(json.dumps(manifest))
        def package(license_dir=notices):
            return PACKAGER.package(self.binary, "x86_64-unknown-linux-gnu", self.licenses,
                self.base / "dist", root=self.source, encoder_helper=self.binary,
                cast_helper=self.binary, cast_licenses=license_dir)
        with tarfile.open(package()) as tar:
            self.assertEqual(tar.getmember(f"{PLUGIN_ID}/omarchy-plugin/native/bin/omabeam-cast").mode, 0o755)
            self.assertIn(f"{PLUGIN_ID}/licenses/cast/LICENSE", tar.getnames())
        with self.assertRaisesRegex(ValueError, "together"):
            package(None)
        manifest["binary"]["sha256"] = "wrong"
        (notices / "manifest.json").write_text(json.dumps(manifest))
        with self.assertRaisesRegex(ValueError, "does not match"):
            package()
        manifest["binary"]["sha256"] = hashlib.sha256(self.binary.read_bytes()).hexdigest()
        (notices / "manifest.json").write_text(json.dumps(manifest))
        license_file.write_text("modified notice")
        with self.assertRaisesRegex(ValueError, "checksum"):
            package()
        manifest["files"][0]["path"] = "../THIRDPARTY.yml"
        (notices / "manifest.json").write_text(json.dumps(manifest))
        with self.assertRaisesRegex(ValueError, "Unsafe"):
            package()

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
        _, firewall_log = mock_firewall(tools)
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
import os, sys
from pathlib import Path
assert "--locked" in sys.argv
root = Path(sys.argv[sys.argv.index("--root") + 1])
name = "omabeam-encoder" if Path(sys.argv[sys.argv.index("--path") + 1]).name == "omabeam-encoder" else "omabeam"
if name == "omabeam-encoder" and os.environ.get("OMABEAM_TEST_HELPER_FAIL"): sys.exit(1)
path = root / "bin" / name
path.parent.mkdir(parents=True, exist_ok=True)
path.write_text({native!r})
path.chmod(0o755)
''')
        env = {**os.environ, "HOME": str(home), "XDG_CONFIG_HOME": str(config), "XDG_CACHE_HOME": str(home / "cache"), "PATH": f"{tools}:{os.environ['PATH']}"}
        def install(path, *args, success=True):
            result = subprocess.run(["bash", str(path / "install.sh"), *args], env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
            return result
        env["OMABEAM_TEST_HELPER_FAIL"] = "1"
        result = install(self.source, "--backend-only")
        self.assertIn("UDP 9848 (WebRTC): ALLOWED", result.stdout)
        self.assertIn("Hardware encoder helper could not be built", result.stdout)
        self.assertTrue((self.source / "omarchy-plugin/native/bin/omabeam").is_file())
        self.assertFalse((self.source / "omarchy-plugin/native/bin/omabeam-encoder").exists())
        env.pop("OMABEAM_TEST_HELPER_FAIL")
        self.assertFalse((config / "omarchy").exists())
        install(self.source)
        installed = config / "omarchy/plugins" / PLUGIN_ID
        self.assertTrue((installed / "omarchy-plugin/native/bin/omabeam").is_file())
        self.assertTrue((installed / "omarchy-plugin/native/bin/omabeam-encoder").is_file())
        self.assertTrue((installed / "RELEASING.md").is_file())
        self.assertTrue((installed / "omarchy-plugin/firewall.py").is_file())
        self.assertIn(str(installed / "omarchy-plugin/omabeam"), (hypr / "bindings.lua").read_text())
        self.assertIn("json.dumps", Path(ROOT / "install.sh").read_text())  # launch path is quoted
        before = [(hypr / name).read_text() for name in ("hyprland.lua", "bindings.lua")]
        # Copied runtime-only installs can rerun without Cargo/source present.
        (tools / "cargo").unlink()
        install(installed)
        self.assertIn("UDP 9848 (WebRTC): ALLOWED", install(installed, "--check-ports").stdout)
        mutations = [json.loads(line) for line in firewall_log.read_text().splitlines() if json.loads(line)[0] != "status"]
        self.assertEqual(len(mutations), 2)
        self.assertIn("UDP 9848 (WebRTC): ALLOWED", install(installed, "--backend-only", "--open-firewall", "192.168.1.0/24").stdout)
        self.assertEqual(len([json.loads(line) for line in firewall_log.read_text().splitlines() if json.loads(line)[0] != "status"]), 2)
        self.assertEqual(before, [(hypr / name).read_text() for name in ("hyprland.lua", "bindings.lua")])
        (installed / ".git").mkdir()
        (installed / ".git/sentinel").write_text("keep")
        self.assertIn("Git-managed", install(self.source, success=False).stderr)
        install(installed)
        self.assertEqual((installed / ".git/sentinel").read_text(), "keep")
        install(installed, "--remove-desktop")
        bindings = (hypr / "bindings.lua").read_text()
        self.assertNotIn("OmaBeam", bindings)
        self.assertNotIn("omabeam (install.sh)", (hypr / "hyprland.lua").read_text())

    def test_installer_check_only_scoped_open_and_invalid_arguments(self):
        home, tools = self.base / "home", self.base / "tools"
        state, log = mock_firewall(tools)
        executable(tools / "uname", "#!/bin/sh\necho Linux\n")
        # Any accidental build or desktop command must fail this test.
        for name in ("cargo", "omarchy", "omarchy-shell", "rsync", "wl-copy", "jq"):
            executable(tools / name, "#!/bin/sh\necho unexpected-install-command >&2\nexit 99\n")
        env = {**os.environ, "HOME": str(home), "XDG_CONFIG_HOME": str(home / "config"), "PATH": f"{tools}:{os.environ['PATH']}"}

        def install(*args):
            return subprocess.run(["bash", str(self.source / "install.sh"), *args], env=env, capture_output=True, text=True, timeout=10)

        for args in (("--open-firewall", "0.0.0.0/0"), ("--subnet", "invalid"),
                     ("--check-ports", "--backend-only"), ("--remove-desktop", "--open-firewall", "192.168.1.0/24"),
                     ("--open-firewall",), ("--subnet", "10.0.0.0/8", "--open-firewall", "192.168.1.0/24")):
            with self.subTest(args=args):
                result = install(*args)
                self.assertNotEqual(result.returncode, 0)
                self.assertNotIn("unexpected-install-command", result.stderr)
                self.assertFalse(log.exists())
        result = install("--check-ports")
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("TCP 9847 (browser / JPEG): BLOCKED", result.stdout)
        self.assertIn("UDP 9848 (WebRTC): BLOCKED", result.stdout)
        self.assertEqual(json.loads(state.read_text()), [])
        for _ in range(2):
            result = install("--check-ports", "--open-firewall", "192.168.1.0/24")
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertIn("UDP 9848 (WebRTC): ALLOWED", result.stdout)
        self.assertCountEqual(json.loads(state.read_text()), [
            ["192.168.1.0/24", "9847", "tcp"], ["192.168.1.0/24", "9848", "udp"]])
        mutations = [json.loads(line) for line in log.read_text().splitlines() if json.loads(line)[0] != 'status']
        self.assertEqual(len(mutations), 2)
        self.assertFalse(home.exists())
        self.assertFalse((self.source / "omarchy-plugin/native").exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
