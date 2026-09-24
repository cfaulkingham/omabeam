#!/usr/bin/env python3
"""Exercise the piped release installer with a real terminal and fake system tools."""
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import subprocess
import sys
import tempfile
import termios
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]


class Bootstrap(unittest.TestCase):
    def setUp(self):
        if os.geteuid() == 0:
            self.skipTest("The release installer deliberately rejects root")
        temporary = tempfile.TemporaryDirectory(prefix="omabeam-bootstrap-")
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name)
        self.tools = self.base / "tools"
        self.tools.mkdir()
        (self.base / "tmp").mkdir()
        self.log = self.base / "calls.jsonl"
        self.env = {**os.environ, "HOME": str(self.base / "home"),
                    "XDG_CONFIG_HOME": str(self.base / "config"),
                    "TMPDIR": str(self.base / "tmp"),
                    "PATH": f"{self.tools}:{os.environ['PATH']}",
                    "OMABEAM_TEST_LOG": str(self.log)}
        self.fake = '''import json, os, sys
from pathlib import Path
name = Path(sys.argv[0]).name
with open(os.environ["OMABEAM_TEST_LOG"], "a") as log:
    log.write(json.dumps([name, sys.argv[1:], sys.stdin.isatty()]) + "\\n")
if name == "uname":
    print(os.environ.get("OMABEAM_TEST_OS", "Linux") if sys.argv[1] == "-s" else os.environ.get("OMABEAM_TEST_ARCH", "x86_64"))
elif name == "pacman":
    assert sys.argv[1:] == ["-Qq", "base-devel"]
    sys.exit(int(os.environ.get("OMABEAM_TEST_BASE_MISSING", "0")))
elif name == "curl":
    Path(sys.argv[sys.argv.index("--output") + 1]).write_text("# downloaded recipe\\n")
    sys.exit(int(os.environ.get("OMABEAM_TEST_DOWNLOAD_STATUS", "0")))
elif name == "sudo":
    assert sys.argv[1:] == ["pacman", "-S", "--needed", "base-devel"]
    assert sys.stdin.isatty()
elif name == "makepkg":
    assert sys.argv[1:] == ["-si", "--needed"]
    assert sys.stdin.isatty()
    assert Path("PKGBUILD").read_text() == "# downloaded recipe\\n"
    if os.environ.get("OMABEAM_TEST_PROMPT"):
        print("Proceed with package installation?", flush=True)
        assert sys.stdin.readline().strip() == "yes"
    sys.exit(int(os.environ.get("OMABEAM_TEST_PACKAGE_STATUS", "0")))
'''
        for name in ("uname", "pacman", "curl", "sudo", "makepkg", "omarchy", "omarchy-shell"):
            script = self.tools / name
            script.write_text(f"#!{sys.executable}\n" + self.fake)
            script.chmod(0o755)
        setup = self.base / "setup.sh"
        setup.write_text(f'''#!/usr/bin/env bash
[[ -t 0 ]] || exit 99
printf '%s\\n' '["setup", [], true]' >> "$OMABEAM_TEST_LOG"
''')
        # Repoint only the system installer in this test copy; no real install.
        self.script = (ROOT / "install-release.sh").read_text().replace(
            "/usr/share/omabeam/plugin/install.sh", f'"{setup}"')

    def calls(self):
        return [json.loads(line)[0] for line in self.log.read_text().splitlines()] if self.log.exists() else []

    def run_pipe(self, *, terminal=True, timeout=15):
        if not terminal:
            result = subprocess.run(["bash"], input=self.script, text=True,
                                    capture_output=True, env=self.env, start_new_session=True, timeout=10)
            return result.returncode, result.stdout + result.stderr
        master, slave = pty.openpty()

        def controlling_terminal():
            os.setsid()
            fcntl.ioctl(1, termios.TIOCSCTTY, 0)

        process = subprocess.Popen(["bash"], stdin=subprocess.PIPE, stdout=slave,
                                   stderr=slave, env=self.env, preexec_fn=controlling_terminal)
        os.close(slave)
        output = b""
        answered = False
        try:
            process.stdin.write(self.script.encode())
            process.stdin.close()
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                if select.select([master], [], [], 0.1)[0]:
                    try:
                        chunk = os.read(master, 65536)
                    except OSError as error:
                        if error.errno == errno.EIO:
                            break
                        raise
                    if not chunk:
                        break
                    output += chunk
                    if b"Proceed with package installation?" in output and not answered:
                        os.write(master, b"yes\n")
                        answered = True
                elif process.poll() is not None:
                    break
            return process.wait(timeout=1), output.decode(errors="replace")
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            os.close(master)

    def test_piped_install_keeps_prompts_on_terminal_and_cleans_up(self):
        for arch in ("x86_64", "aarch64"):
            with self.subTest(arch=arch):
                self.env.update(OMABEAM_TEST_ARCH=arch, OMABEAM_TEST_PROMPT="1")
                self.log.unlink(missing_ok=True)
                code, output = self.run_pipe()
                self.assertEqual(code, 0, output)
                self.assertEqual(self.calls()[-2:], ["makepkg", "setup"])
                self.assertNotIn("sudo", self.calls())
                self.assertEqual(list((self.base / "tmp").iterdir()), [])

    def test_missing_build_tools_are_installed_before_makepkg(self):
        self.env["OMABEAM_TEST_BASE_MISSING"] = "1"
        code, output = self.run_pipe()
        self.assertEqual(code, 0, output)
        self.assertEqual(self.calls()[-3:], ["sudo", "makepkg", "setup"])

    def test_failed_download_or_package_never_runs_setup_and_cleans_up(self):
        for variable in ("OMABEAM_TEST_DOWNLOAD_STATUS", "OMABEAM_TEST_PACKAGE_STATUS"):
            with self.subTest(variable=variable):
                self.env[variable] = "22"
                self.log.unlink(missing_ok=True)
                code, _ = self.run_pipe()
                self.assertEqual(code, 22)
                self.assertNotIn("setup", self.calls())
                if "DOWNLOAD" in variable:
                    self.assertNotIn("sudo", self.calls())
                    self.assertNotIn("makepkg", self.calls())
                self.assertEqual(list((self.base / "tmp").iterdir()), [])
                del self.env[variable]

    def test_unsupported_platform_and_no_terminal_stop_before_download(self):
        for setting, value in (("OMABEAM_TEST_OS", "Darwin"), ("OMABEAM_TEST_ARCH", "riscv64"), (None, None)):
            with self.subTest(setting=setting):
                if setting:
                    self.env[setting] = value
                self.log.unlink(missing_ok=True)
                code, output = self.run_pipe(terminal=False)
                self.assertNotEqual(code, 0, output)
                self.assertNotIn("curl", self.calls())
                if setting:
                    del self.env[setting]
                else:
                    self.assertIn("terminal", output)

    def test_existing_git_plugin_is_preserved_before_package_changes(self):
        plugin = Path(self.env["XDG_CONFIG_HOME"]) / "omarchy/plugins/io.github.cfaulkingham.omabeam"
        plugin.mkdir(parents=True)
        (plugin / ".git").write_text("gitdir: a-worktree\n")
        code, output = self.run_pipe()
        self.assertNotEqual(code, 0)
        self.assertIn("Git-managed", output)
        self.assertNotIn("curl", self.calls())
        self.assertEqual((plugin / ".git").read_text(), "gitdir: a-worktree\n")


if __name__ == "__main__":
    unittest.main(verbosity=2)
