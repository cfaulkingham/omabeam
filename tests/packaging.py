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


def native_app_script():
    """Mock native app written by the fake `cargo install` below. Without
    OMABEAM_TEST_HYPR_STATE naming a state directory, it keeps the behavior
    test_installer_build_bundle_upgrade_and_git_checkout relies on: --hypr
    version fails, so install.sh skips the reload entirely. With it, it
    simulates the real CLI's --hypr version/reload/configerrors contract so
    the config-error-baseline path in edit_hypr can be exercised: reload
    copies errors-after.json over errors-current.json (if staged), and
    configerrors always reports whatever errors-current.json holds (or []).
    """
    return f'''#!{sys.executable}
import os
import shutil
import sys
from pathlib import Path

args = sys.argv[1:]
state = os.environ.get("OMABEAM_TEST_HYPR_STATE")
log = os.environ.get("OMABEAM_TEST_LOG")


def log_call(entry):
    if log:
        with open(log, "a") as stream:
            stream.write(entry + "\\n")


if not state:
    if args == ["--hypr", "version"]:
        sys.exit(1)
    print("native app")
    sys.exit(0)

directory = Path(state)
if args == ["--hypr", "version"]:
    sys.exit(0)
if args == ["--hypr", "reload"]:
    after = directory / "errors-after.json"
    if after.exists():
        shutil.copyfile(after, directory / "errors-current.json")
    log_call("reload")
    print("ok")
    sys.exit(0)
if args == ["--hypr", "configerrors"]:
    current = directory / "errors-current.json"
    print(current.read_text() if current.exists() else "[]")
    sys.exit(0)
print("native app")
'''


def install_env(base, *, hypr_state=None, log=None):
    """Shared harness for exercising install.sh end to end: a disposable
    HOME/config plus mocked uname/wl-copy/omarchy/omarchy-shell/cargo/ufw
    (see test_installer_build_bundle_upgrade_and_git_checkout for the
    pattern this extends). Optionally wires the Hyprland-reload state
    directory and shared call log that the native/omarchy/omarchy-shell
    mocks use to simulate a reachable compositor and record call order."""
    home, tools = base / "home", base / "tools"
    tools.mkdir()
    mock_firewall(tools)
    config = home / "config with spaces"
    hypr = config / "hypr"
    hypr.mkdir(parents=True)
    (hypr / "hyprland.lua").write_text("-- user config\n")
    (hypr / "bindings.lua").write_text("-- user binding\n")
    executable(tools / "uname", "#!/bin/sh\necho Linux\n")
    executable(tools / "wl-copy", "#!/bin/sh\nexit 0\n")
    executable(tools / "omarchy-shell", f'''#!{sys.executable}
import os
import sys
log = os.environ.get("OMABEAM_TEST_LOG")
if log:
    with open(log, "a") as stream:
        stream.write(" ".join(sys.argv[1:]) + "\\n")
''')
    executable(tools / "omarchy", f'''#!{sys.executable}
import json
import os
import sys
from pathlib import Path
log = os.environ.get("OMABEAM_TEST_LOG")
if log:
    with open(log, "a") as stream:
        stream.write(" ".join(sys.argv[1:]) + "\\n")
if sys.argv[1:3] == ["plugin", "validate"]:
    root = Path(sys.argv[3])
    manifest = json.loads((root / "manifest.json").read_text())
    assert (root / manifest["entryPoints"]["barWidget"]).is_file()
''')
    native = native_app_script()
    executable(tools / "cargo", f'''#!{sys.executable}
import sys
from pathlib import Path
assert "--locked" in sys.argv
root = Path(sys.argv[sys.argv.index("--root") + 1])
name = "omabeam-encoder" if Path(sys.argv[sys.argv.index("--path") + 1]).name == "omabeam-encoder" else "omabeam"
path = root / "bin" / name
path.parent.mkdir(parents=True, exist_ok=True)
path.write_text({native!r})
path.chmod(0o755)
''')
    env = {**os.environ, "HOME": str(home), "XDG_CONFIG_HOME": str(config),
           "XDG_CACHE_HOME": str(home / "cache"), "PATH": f"{tools}:{os.environ['PATH']}"}
    if hypr_state is not None:
        hypr_state.mkdir(parents=True, exist_ok=True)
        env["OMABEAM_TEST_HYPR_STATE"] = str(hypr_state)
    if log is not None:
        env["OMABEAM_TEST_LOG"] = str(log)
    return env, hypr


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
        self.assertEqual(archive.stat().st_mode & 0o777, 0o644)
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

    def run_install(self, source, *args, env, success=True):
        result = subprocess.run(["bash", str(source / "install.sh"), *args], env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode == 0, success, result.stdout + result.stderr)
        return result

    def test_hypr_reload_baseline_ignores_preexisting_errors_and_reloads_before_restart(self):
        state = self.base / "hypr-state"
        log = self.base / "install-log.txt"
        env, hypr = install_env(self.base, hypr_state=state, log=log)
        (state / "errors-current.json").write_text(json.dumps(["config error at line 3: bad value"]))
        result = self.run_install(self.source, env=env)
        self.assertIn("Hyprland already reported these configuration errors before OmaBeam changed anything:", result.stdout)
        self.assertIn("config error at line 3: bad value", result.stdout)
        self.assertIn('o.window("omabeam"', (hypr / "hyprland.lua").read_text())
        calls = log.read_text().splitlines()
        self.assertLess(calls.index("reload"), calls.index("restart shell"))

    def test_hypr_reload_rejects_new_config_errors_and_rolls_back(self):
        state = self.base / "hypr-state"
        log = self.base / "install-log.txt"
        env, hypr = install_env(self.base, hypr_state=state, log=log)
        (state / "errors-after.json").write_text(json.dumps(["config error at line 90: omabeam broke it"]))
        before = {name: (hypr / name).read_text() for name in ("hyprland.lua", "bindings.lua")}
        result = self.run_install(self.source, env=env, success=False)
        self.assertIn("config error at line 90: omabeam broke it", result.stderr)
        self.assertEqual(before, {name: (hypr / name).read_text() for name in ("hyprland.lua", "bindings.lua")})
        installed = Path(env["XDG_CONFIG_HOME"]) / "omarchy/plugins" / PLUGIN_ID
        self.assertFalse(installed.exists())
        calls = log.read_text().splitlines() if log.exists() else []
        self.assertFalse(any("plugin enable" in call for call in calls))
        self.assertNotIn("restart shell", calls)

    def test_remove_desktop_config_error_baseline_allows_preexisting_errors(self):
        state = self.base / "hypr-state"
        env, hypr = install_env(self.base, hypr_state=state)
        # --remove-desktop never builds; it needs an already-built native
        # binary on $ROOT to reach Hyprland at all (the same one a real
        # install would have left behind), otherwise reload is skipped.
        self.run_install(self.source, "--backend-only", env=env)
        (hypr / "hyprland.lua").write_text(
            '-- user config\n'
            '-- omabeam (install.sh)\n'
            'o.window("omabeam", {\n'
            '  float = true,\n'
            '  center = true,\n'
            '  focus_on_activate = false,\n'
            '  animation = "popin",\n'
            '  size = { "(monitor_w*3/4)", "(monitor_h*3/4)" },\n'
            '  max_size = { 980, 560 },\n'
            '})\n'
        )
        (hypr / "bindings.lua").write_text(
            '-- user binding\n-- omabeam (install.sh)\n'
            'o.bind("SUPER + SHIFT + T", "OmaBeam", { launch = "/old/bin/omabeam" })\n'
        )
        (state / "errors-current.json").write_text(json.dumps(["config error at line 12: user typo"]))
        (state / "errors-after.json").write_text(json.dumps(["config error at line 8: user typo"]))
        result = self.run_install(self.source, "--remove-desktop", env=env)
        self.assertIn("Hyprland already reported these configuration errors before OmaBeam changed anything:", result.stdout)
        self.assertNotIn("OmaBeam", (hypr / "bindings.lua").read_text())
        self.assertNotIn("omabeam (install.sh)", (hypr / "hyprland.lua").read_text())

    def test_window_rules_float_the_picker_and_the_send_window(self):
        env, hypr = install_env(self.base)
        # An installation from before the send window had its own app id.
        picker_only = (
            '-- user config\n'
            '-- omabeam (install.sh)\n'
            'o.window("omabeam", {\n'
            '  float = true,\n'
            '  center = true,\n'
            '  focus_on_activate = false,\n'
            '  animation = "popin",\n'
            '  size = { "(monitor_w*3/4)", "(monitor_h*3/4)" },\n'
            '  max_size = { 980, 560 },\n'
            '})\n'
        )
        (hypr / "hyprland.lua").write_text(picker_only)
        self.run_install(self.source, env=env)
        upgraded = (hypr / "hyprland.lua").read_text()
        self.assertEqual(upgraded.count("-- omabeam (install.sh)"), 1, upgraded)
        self.assertEqual(upgraded.count('o.window("omabeam", {'), 1, upgraded)
        self.assertEqual(upgraded.count('o.window("omabeam-send", {'), 1, upgraded)
        send_rule = upgraded[upgraded.index('o.window("omabeam-send", {'):]
        self.assertIn("float = true", send_rule)
        self.assertIn("center = true", send_rule)
        self.assertIn("size = { 440, 560 }", send_rule)
        # After the picker's rule, so the send window's size wins even where
        # the picker's class would also match it.
        self.assertLess(upgraded.index('o.window("omabeam", {'), upgraded.index('o.window("omabeam-send", {'))
        # Reinstalling keeps a single up-to-date block.
        self.run_install(self.source, env=env)
        self.assertEqual((hypr / "hyprland.lua").read_text(), upgraded)
        self.run_install(self.source, "--remove-desktop", env=env)
        removed = (hypr / "hyprland.lua").read_text()
        self.assertNotIn("omabeam", removed)
        self.assertIn("-- user config", removed)

    def test_symlinked_hyprland_lua_is_not_edited_and_prints_both_blocks(self):
        env, hypr = install_env(self.base)
        external = self.base / "external-hyprland.lua"
        external.write_text("-- externally managed by chezmoi\n")
        (hypr / "hyprland.lua").unlink()
        (hypr / "hyprland.lua").symlink_to(external)
        bindings_before = (hypr / "bindings.lua").read_text()
        result = self.run_install(self.source, env=env)
        # Only hyprland.lua is linked, but bindings.lua is left unedited too
        # (edit_hypr never edits one file without the other), so the printed
        # guidance must cover both files, not just the one that tripped it.
        self.assertIn('o.window("omabeam"', result.stdout)
        self.assertIn('o.window("omabeam-send"', result.stdout)
        self.assertIn("o.bind(", result.stdout)
        self.assertIn("bindings.lua", result.stdout)
        self.assertTrue((hypr / "hyprland.lua").is_symlink())
        self.assertEqual(os.readlink(hypr / "hyprland.lua"), str(external))
        self.assertEqual(external.read_text(), "-- externally managed by chezmoi\n")
        self.assertEqual((hypr / "bindings.lua").read_text(), bindings_before)
        # The banner must not claim a key bind that was never added.
        self.assertNotIn("SUPER + SHIFT + T  or", result.stdout)
        self.assertNotIn("Traceback", result.stdout)
        self.assertNotIn("Traceback", result.stderr)

    def test_symlinked_bindings_lua_is_not_edited_and_prints_both_blocks(self):
        env, hypr = install_env(self.base)
        external = self.base / "external-bindings.lua"
        external.write_text("-- externally managed by chezmoi\n")
        (hypr / "bindings.lua").unlink()
        (hypr / "bindings.lua").symlink_to(external)
        hyprland_before = (hypr / "hyprland.lua").read_text()
        result = self.run_install(self.source, env=env)
        # The mirror image of the case above: only bindings.lua is linked,
        # but hyprland.lua's window-rule guidance must still be printed.
        self.assertIn('o.window("omabeam"', result.stdout)
        self.assertIn("o.bind(", result.stdout)
        self.assertIn("hyprland.lua", result.stdout)
        self.assertTrue((hypr / "bindings.lua").is_symlink())
        self.assertEqual(os.readlink(hypr / "bindings.lua"), str(external))
        self.assertEqual(external.read_text(), "-- externally managed by chezmoi\n")
        self.assertEqual((hypr / "hyprland.lua").read_text(), hyprland_before)
        self.assertNotIn("SUPER + SHIFT + T  or", result.stdout)
        self.assertNotIn("Traceback", result.stdout)
        self.assertNotIn("Traceback", result.stderr)

    def test_remove_desktop_with_symlinked_bindings_lua_fails_without_editing(self):
        env, hypr = install_env(self.base)
        external = self.base / "external-bindings.lua"
        external.write_text(
            '-- user binding\n-- omabeam (install.sh)\n'
            'o.bind("SUPER + SHIFT + T", "OmaBeam", { launch = "/old/bin/omabeam" })\n'
        )
        hypr_before = (hypr / "hyprland.lua").read_text()
        external_before = external.read_text()
        (hypr / "bindings.lua").unlink()
        (hypr / "bindings.lua").symlink_to(external)
        result = self.run_install(self.source, "--remove-desktop", env=env, success=False)
        self.assertIn("-- omabeam (install.sh)", result.stderr)
        self.assertIn("by hand", result.stderr)
        # Only bindings.lua is linked, but the message must still name both
        # files: neither one is touched, so the user must clean up both.
        self.assertIn("hyprland.lua", result.stderr)
        self.assertIn("bindings.lua", result.stderr)
        self.assertNotIn("Traceback", result.stdout)
        self.assertNotIn("Traceback", result.stderr)
        self.assertEqual((hypr / "hyprland.lua").read_text(), hypr_before)
        self.assertTrue((hypr / "bindings.lua").is_symlink())
        self.assertEqual(external.read_text(), external_before)

    def test_hardware_encoder_helper_warns_only_when_ldd_reports_missing_libraries(self):
        env, hypr = install_env(self.base)
        tools = self.base / "tools"
        executable(tools / "ldd", "#!/bin/sh\nprintf '\\tlibavcodec.so.60 => not found\\n\\tlibavutil.so.58 => not found\\n'\n")
        result = self.run_install(self.source, "--backend-only", env=env)
        self.assertIn("hardware encoder helper cannot load", result.stdout)
        self.assertIn("libavcodec.so.60", result.stdout)
        self.assertIn("libavutil.so.58", result.stdout)
        executable(tools / "ldd", "#!/bin/sh\nprintf '\\tlibc.so.6 => /lib/libc.so.6 (0x1)\\n'\n")
        result = self.run_install(self.source, "--backend-only", env=env)
        self.assertNotIn("hardware encoder helper cannot load", result.stdout)
        # A non-dynamic executable makes real `ldd` exit non-zero; the check
        # must stay quiet and must not abort the script under `set -euo pipefail`.
        executable(tools / "ldd", "#!/bin/sh\necho 'not a dynamic executable' >&2\nexit 1\n")
        result = self.run_install(self.source, "--backend-only", env=env)
        self.assertNotIn("hardware encoder helper cannot load", result.stdout)
        self.assertNotIn("not a dynamic executable", result.stdout + result.stderr)

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
