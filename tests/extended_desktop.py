#!/usr/bin/env python3
"""Real CLI lifecycle against a simulated Hyprland socket; no host display edits."""
import argparse
import fcntl
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest

BINARY = str(Path('target/debug/omabeam').resolve())
# --stop and --status identify share processes through /proc.
LINUX = sys.platform.startswith('linux')


def wait_for(check, timeout):
    deadline = time.monotonic() + timeout
    while not check():
        if time.monotonic() >= deadline:
            return False
        time.sleep(.01)
    return True


def all_threads_stopped(pid):
    """Every thread of `pid` is in a job-control stop (SIGSTOP)."""
    for task in Path(f'/proc/{pid}/task').iterdir():
        try:
            state = Path(task, 'stat').read_text().rsplit(')', 1)[1].split()[0]
        except (FileNotFoundError, ProcessLookupError):
            continue  # The thread exited after the listing; it cannot run.
        if state != 'T':
            return False
    return True


def signal_pending(pid, sig):
    """`sig` was sent to `pid` and not yet handled."""
    return any(line.startswith(('ShdPnd:', 'SigPnd:')) and int(line.split(':', 1)[1], 16) >> (sig - 1) & 1
               for line in Path(f'/proc/{pid}/status').read_text().splitlines())


def monitor(name, width=1920, height=1080, scale=1, x=0, y=0):
    return dict(id=0, name=name, width=width, height=height, scale=scale, x=x, y=y,
                focused=True, activeWorkspace=dict(id=1, name='1'))


class Compositor:
    def __init__(self, root):
        directory = root / 'hypr/fixture'
        directory.mkdir(parents=True)
        self.socket = socket.socket(socket.AF_UNIX)
        self.socket.bind(str(directory / '.socket.sock'))
        self.socket.listen()
        self.socket.settimeout(.1)
        self.outputs = {'DP-1': monitor('DP-1')}
        self.commands = []
        self.created = threading.Event()
        self.release = threading.Event()
        self.stopping = threading.Event()
        self.hold_create = False
        self.fail_create = False
        self.fail_config = False
        self.fail_next_config = False
        self.fail_remove = False
        self.error = None
        self.thread = threading.Thread(target=self.serve)
        self.thread.start()

    def serve(self):
        try:
            while not self.stopping.is_set():
                try:
                    client, _ = self.socket.accept()
                except socket.timeout:
                    continue
                with client:
                    request = client.recv(8192).decode()
                    self.commands.append(request)
                    if not request:
                        # is_listening() only probes that something accepts the
                        # connection: it connects and disconnects without
                        # writing a request, expecting no reply.
                        continue
                    reply = 'ok'
                    if request in ('j/monitors', 'j/monitors all'):
                        reply = json.dumps(list(self.outputs.values()))
                    elif request.startswith('/output create headless '):
                        name = request.split()[-1]
                        assert re.fullmatch('OMABEAM-[0-9a-f]{32}', name), name
                        assert name not in self.outputs
                        if self.fail_create:
                            reply = 'no backend replied to the request'
                        else:
                            self.outputs[name] = monitor(name)
                            self.created.set()
                            if self.hold_create:
                                self.release.wait(10)
                    elif request.startswith('/eval hl.monitor('):
                        extra = re.fullmatch(r'/eval hl.monitor\(\{ output = "(OMABEAM-[0-9a-f]{32})", mode = "(\d+)x(\d+)@60", position = "(-?\d+)x(-?\d+)", scale = (\d+) \}\)', request)
                        pin = re.fullmatch(r'/eval hl.monitor\(\{ output = "([A-Za-z0-9._-]+)", mode = "(\d+)x(\d+)@[\d.]+", position = "(-?\d+)x(-?\d+)", scale = [\d.]+, transform = (\d+) \}\)', request)
                        if extra:
                            name, w, h, x, y, scale = extra.groups()
                            if self.fail_config or self.fail_next_config:
                                self.fail_next_config = False
                                reply = 'configuration rejected'
                            else:
                                self.outputs[name] = monitor(name, int(w), int(h), int(scale), int(x), int(y))
                        elif pin:
                            name, w, h, x, y, transform = pin.groups()
                            assert name in self.outputs and not name.startswith('OMABEAM-'), name
                            current = self.outputs[name]
                            self.outputs[name] = monitor(name, int(w), int(h), current['scale'], int(x), int(y))
                        else:
                            raise AssertionError(request)
                    elif request.startswith('/output remove '):
                        name = request.split()[-1]
                        assert name != 'DP-1'
                        if self.fail_remove:
                            reply = 'remove failed'
                        else:
                            self.outputs.pop(name, None)
                    elif request == '/reload':
                        pass  # reply stays 'ok'; recorded in commands like every request
                    else:
                        raise AssertionError(f'unexpected IPC request: {request}')
                    try:
                        client.sendall(reply.encode())
                    except BrokenPipeError:
                        pass  # Expected when testing an interrupted create.
        except BaseException as error:
            self.error = error

    def close(self):
        self.stopping.set()
        self.release.set()
        self.thread.join(2)
        self.socket.close()
        if self.error:
            raise self.error


class ExtendedDesktop(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='ob-ext-', dir='/tmp')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.compositor = Compositor(self.root)
        self.addCleanup(self.compositor.close)
        self.env = {**os.environ, 'XDG_RUNTIME_DIR': str(self.root),
                    'HYPRLAND_INSTANCE_SIGNATURE': 'fixture', 'WAYLAND_DISPLAY': 'missing-wayland'}
        self.env.pop('WAYLAND_SOCKET', None)
        self.journal = self.root / 'omabeam/display.json'

    def run_cli(self, *args):
        return subprocess.run([BINARY, *args], env=self.env, capture_output=True, text=True, timeout=12)

    def start(self):
        return self.run_cli('--port', '0', '--live', 'extend', '2560', '1440', '2', 'left')

    def test_capture_start_failure_rolls_back_only_the_created_display(self):
        result = self.start()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('capture initialization failed', result.stderr)
        commands = self.compositor.commands
        create = next(i for i, command in enumerate(commands) if command.startswith('/output create headless '))
        pins = [i for i, command in enumerate(commands) if 'output = "DP-1"' in command]
        # create() pins once, before creating the output; resize() no longer re-pins.
        self.assertEqual(len(pins), 1, commands)
        self.assertLess(pins[0], create, commands)
        self.assertIn('position = "0x0"', commands[pins[0]])
        self.assertTrue(any('position = "-1280x0"' in command for command in commands))
        self.assertEqual(list(self.compositor.outputs), ['DP-1'])
        self.assertFalse(self.journal.exists())
        removes = [i for i, command in enumerate(commands) if command.startswith('/output remove ')]
        self.assertTrue(removes, commands)
        self.assertEqual(commands[-1], '/reload')
        self.assertLess(removes[-1], commands.index('/reload'), commands)

    def test_create_or_configuration_rejection_rolls_back(self):
        for flag in ('fail_create', 'fail_config'):
            with self.subTest(flag=flag):
                setattr(self.compositor, flag, True)
                self.assertNotEqual(self.start().returncode, 0)
                self.assertEqual(list(self.compositor.outputs), ['DP-1'])
                self.assertFalse(self.journal.exists())
                setattr(self.compositor, flag, False)

    def test_failed_removal_preserves_record_and_stop_retries(self):
        self.compositor.fail_remove = True
        result = self.start()
        self.assertIn('Run omabeam --stop', result.stderr)
        self.assertTrue(self.journal.exists())
        self.assertNotIn('/reload', self.compositor.commands)
        self.compositor.fail_remove = False
        result = self.run_cli('--stop')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(list(self.compositor.outputs), ['DP-1'])
        self.assertFalse(self.journal.exists())
        commands = self.compositor.commands
        removes = [i for i, command in enumerate(commands) if command.startswith('/output remove ')]
        self.assertTrue(removes, commands)
        self.assertEqual(commands[-1], '/reload')
        self.assertLess(removes[-1], commands.index('/reload'), commands)

    def test_killed_start_is_recovered_by_stop_even_from_a_different_session(self):
        self.compositor.hold_create = True
        process = subprocess.Popen([BINARY, '--port', '0', '--live', 'extend', '1920', '1080', '1', 'right'],
            env=self.env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        try:
            self.assertTrue(self.compositor.created.wait(5))
            process.kill()
            process.wait(5)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            self.compositor.release.set()
        self.assertTrue(self.journal.exists())
        self.env['HYPRLAND_INSTANCE_SIGNATURE'] = 'another-session'
        result = self.run_cli('--stop')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(list(self.compositor.outputs), ['DP-1'])
        self.assertFalse(self.journal.exists())
        commands = self.compositor.commands
        removes = [i for i, command in enumerate(commands) if command.startswith('/output remove ')]
        self.assertTrue(removes, commands)
        self.assertEqual(commands[-1], '/reload')
        self.assertLess(removes[-1], commands.index('/reload'), commands)

    def test_stale_compositor_socket_is_treated_as_exited(self):
        # A crashed compositor leaves its socket file behind (Rust does not
        # unlink on drop either); connecting to it must fail fast rather than
        # hang, and must be treated the same as a missing socket.
        name = 'OMABEAM-' + '0123456789abcdef' * 2
        crashed = self.root / 'hypr/crashed-session'
        crashed.mkdir(parents=True)
        stale = socket.socket(socket.AF_UNIX)
        stale.bind(str(crashed / '.socket.sock'))
        stale.close()

        self.journal.parent.mkdir(mode=0o700, exist_ok=True)
        self.journal.write_text(json.dumps(dict(name=name, instance='crashed-session')))
        self.journal.chmod(0o600)

        result = self.run_cli('--stop')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.journal.exists())
        self.assertEqual(self.compositor.commands, [])

        result = self.run_cli('--stop')
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_start_and_stop_refuse_to_mutate_while_another_session_holds_lock(self):
        directory = self.root / 'omabeam'
        directory.mkdir(mode=0o700)
        with (directory / 'session.lock').open('w') as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            for args in [('--live', 'extend', '1920', '1080', '1', 'right'), ('--stop',)]:
                result = self.run_cli(*args)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('already running or starting', result.stderr)
        self.assertEqual(self.compositor.commands, [])

    def test_a_signal_during_startup_unwinds_before_capture(self):
        self.compositor.hold_create = True
        share = subprocess.Popen([BINARY, '--port', '0', '--live', 'extend', '1920', '1080', '1', 'right'],
            env=self.env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        try:
            self.assertTrue(self.compositor.created.wait(5))
            share.terminate()  # Queued before the compositor answers.
            self.compositor.release.set()
            error = share.communicate(timeout=15)[1]
        finally:
            if share.poll() is None:
                share.kill()
                share.wait()
            self.compositor.release.set()
        self.assertEqual(share.returncode, 0, error)
        self.assertIn('Stopped before the share started.', error)
        self.assertNotIn('capture initialization failed', error)
        self.assertEqual(list(self.compositor.outputs), ['DP-1'])
        self.assertFalse(self.journal.exists())
        self.assertFalse((self.root / 'omabeam/live.json').exists())
        commands = self.compositor.commands
        removes = [i for i, command in enumerate(commands) if command.startswith('/output remove ')]
        self.assertTrue(removes, commands)
        self.assertEqual(commands[-1], '/reload')
        self.assertLess(removes[-1], commands.index('/reload'), commands)

    @unittest.skipUnless(LINUX, 'finding the starting share needs /proc')
    def test_stop_during_startup_ends_the_share_and_removes_its_display(self):
        self.compositor.hold_create = True
        share = subprocess.Popen([BINARY, '--port', '0', '--live', 'extend', '1920', '1080', '1', 'right'],
            env=self.env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
        stop = None
        try:
            self.assertTrue(self.compositor.created.wait(5))
            # Nothing is published yet; --stop must find the share through its lock.
            self.assertFalse((self.root / 'omabeam/live.json').exists())
            # Stopped, the share leaves --stop's SIGTERM pending where the test
            # can see it, instead of handling it at once. Creation is released
            # only after that, so the share cannot reach capture first.
            share.send_signal(signal.SIGSTOP)
            self.assertTrue(wait_for(lambda: all_threads_stopped(share.pid), .5))
            stop = subprocess.Popen([BINARY, '--stop'], env=self.env,
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            # Bounded, so creation is still released well inside the share's
            # 5 s Hyprland IPC timeout.
            signaled = wait_for(lambda: signal_pending(share.pid, signal.SIGTERM), 2.5)
            share.send_signal(signal.SIGCONT)
            self.assertTrue(signaled, '--stop did not signal the starting share')
            self.compositor.release.set()
            stopped, stop_error = stop.communicate(timeout=15)
            share_error = share.communicate(timeout=15)[1]
        finally:
            for process in (share, stop):
                if process is not None and process.poll() is None:
                    process.kill()
                    process.wait()
            self.compositor.release.set()
        self.assertEqual(stop.returncode, 0, stop_error)
        self.assertEqual(stopped.strip(), 'stopped', stop_error)
        # The share saw the stop before capture started, rather than failing there.
        self.assertEqual(share.returncode, 0, share_error)
        self.assertNotIn('capture initialization failed', share_error)
        self.assertEqual(list(self.compositor.outputs), ['DP-1'])
        self.assertFalse(self.journal.exists())
        self.assertFalse((self.root / 'omabeam/live.json').exists())
        commands = self.compositor.commands
        removes = [i for i, command in enumerate(commands) if command.startswith('/output remove ')]
        self.assertTrue(removes, commands)
        self.assertEqual(commands[-1], '/reload')
        self.assertLess(removes[-1], commands.index('/reload'), commands)

    @unittest.skipUnless(LINUX, 'process start times come from /proc')
    def test_a_record_naming_a_reused_pid_is_stale(self):
        sleeper = subprocess.Popen(['sleep', '30'])
        self.addCleanup(sleeper.wait)
        self.addCleanup(sleeper.kill)
        stat = Path(f'/proc/{sleeper.pid}/stat').read_text()
        started = int(stat.rsplit(')', 1)[1].split()[19])
        directory = self.root / 'omabeam'
        directory.mkdir(mode=0o700)
        record = directory / 'live.json'
        # A crashed share's pid, now reused by a process that started later.
        record.write_text(json.dumps(dict(
            pid=sleeper.pid, starttime=started - 1, url='http://127.0.0.1:9847/s/' + '0' * 32 + '/',
            title='Output DP-1', fps=0.0, width=640, height=360, frames=1, uptime=1, viewers=0,
            source='Output DP-1', state='live', error=None)))
        record.chmod(0o600)
        result = self.run_cli('--status')
        self.assertEqual(result.returncode, 1, result.stdout)
        result = self.run_cli('--stop')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.strip(), 'not live')
        self.assertFalse(record.exists())
        self.assertIsNone(sleeper.poll(), 'an unrelated process was signaled')
        self.assertEqual(self.compositor.commands, [])

    def test_invalid_config_and_foreign_display_journal_never_mutate(self):
        result = self.run_cli('--live', 'extend', '1920', '1080', '0', 'right')
        self.assertNotEqual(result.returncode, 0)
        self.journal.parent.mkdir(mode=0o700, exist_ok=True)
        self.journal.write_text(json.dumps(dict(name='DP-1', instance='fixture')))
        self.journal.chmod(0o600)
        result = self.run_cli('--stop')
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.compositor.commands, [])
        self.assertEqual(list(self.compositor.outputs), ['DP-1'])


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', default=BINARY)
    args, remaining = parser.parse_known_args()
    BINARY = str(Path(args.binary).resolve())
    unittest.main(argv=[__file__, *remaining], verbosity=2)
