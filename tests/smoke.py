#!/usr/bin/env python3
"""Exercise OmaBeam's actual binary and JPEG/MJPEG path without a compositor.
Pillow is test-only. --browser additionally uses Playwright Chromium.
"""
import argparse
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from PIL import Image


def eventually(check, timeout=10):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        except (OSError, ValueError, AssertionError) as error:
            last = error
        time.sleep(0.05)
    raise AssertionError(f'condition timed out: {last}')


class Server:
    def __init__(self, binary, args, source=None):
        self.binary, self.args = str(Path(binary).resolve()), args
        self.source = source or ["--demo"]

    def __enter__(self):
        self.runtime = tempfile.TemporaryDirectory(prefix='omabeam-smoke-')
        self.env = {**os.environ, 'XDG_RUNTIME_DIR': self.runtime.name}
        self.log = tempfile.TemporaryFile(mode='w+b')
        self.proc = subprocess.Popen([self.binary, '--bind', '127.0.0.1', '--port', '0', *self.args, *self.source], stdout=self.log, stderr=self.log, env=self.env)
        try:
            def ready():
                if self.proc.poll() is not None:
                    self.log.seek(0)
                    raise RuntimeError(self.log.read().decode())
                path = Path(self.runtime.name) / 'omabeam/live.json'
                if path.exists():
                    self.url = json.loads(path.read_text())['url']
                    return True
            eventually(ready)
            return self
        except BaseException:
            self.__exit__(None, None, None)
            raise

    def __exit__(self, *_):
        self.proc.terminate()
        try:
            self.proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.log.close()
        self.runtime.cleanup()

    def get(self, path):
        try:
            response = urllib.request.urlopen(self.url + path, timeout=3)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            return response.status, response.read()

    def stats(self):
        code, body = self.get('stats')
        assert code == 200
        return json.loads(body)


def decode(data):
    image = Image.open(io.BytesIO(data))
    image.load()
    return image


def frame(stream):
    line = stream.readline()
    while line == b'\r\n':
        line = stream.readline()
    assert line == b'--omabeamframe\r\n', line
    headers = {}
    while (line := stream.readline()) != b'\r\n':
        assert line, 'stream ended in headers'
        key, value = line.decode().split(':', 1)
        headers[key.lower()] = value.strip()
    return decode(stream.read(int(headers['content-length'])))


def browser_check(server, screenshot, executable=None):
    from playwright.sync_api import sync_playwright
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True, executable_path=executable)
        page = browser.new_page(viewport={'width': 480, 'height': 380})
        errors = []
        page.on('pageerror', lambda error: errors.append(str(error)))
        try:
            page.goto(server.url, wait_until='domcontentloaded')
            page.wait_for_function('document.getElementById("view").naturalWidth > 0')
            eventually(lambda: server.stats()['viewers'] == 1)
            page.locator('#viewer-setup').wait_for()
            assert page.locator('#setup-match').is_hidden()
            page.get_by_role('button', name='Keep watching', exact=True).click()
            assert page.locator('#viewer-setup').is_hidden()
            page.get_by_text('Stream diagnostics', exact=True).click()
            page.wait_for_function('document.getElementById("diag-pixels").textContent === "Native pixels"')
            page.wait_for_function('document.querySelector("#client-rows tr td").textContent.startsWith("Viewer ")')
            assert page.locator('#diag-capture').inner_text() == '640×360'
            assert page.locator('#diag-encoded').inner_text() == '640×360'
            assert 'capture fps' in page.locator('#metrics').inner_text()
            assert 'ms' in page.locator('#diag-encode').inner_text()
            assert 'Mbit/s' in page.locator('#diag-bandwidth').inner_text()
            assert page.evaluate('document.documentElement.scrollWidth <= innerWidth')
            if screenshot:
                page.screenshot(path=str(Path(screenshot).with_name('diagnostics.png')))
                page.set_viewport_size({'width': 1280, 'height': 900})
                page.screenshot(path=str(Path(screenshot).with_name('diagnostics-desktop.png')))
                page.set_viewport_size({'width': 480, 'height': 380})
            page.get_by_role('button', name='Pause', exact=True).click()
            eventually(lambda: server.stats()['viewers'] == 0)
            page.get_by_text('No active JPEG connections', exact=True).wait_for()
            assert page.locator('#status').inner_text() == 'Paused'
            page.get_by_text('Stream diagnostics', exact=True).click()
            page.get_by_role('button', name='Resume', exact=True).click()
            page.wait_for_function('document.getElementById("view").naturalWidth > 0')
            eventually(lambda: server.stats()['viewers'] == 1)
            contain = page.locator('#view').bounding_box()
            page.get_by_role('button', name='Fit: contain', exact=True).click()
            width = page.locator('#view').bounding_box()
            page.get_by_role('button', name='Fit: fill width', exact=True).click()
            actual = page.locator('#view').bounding_box()
            assert contain['height'] < width['height'] < actual['height'], (contain, width, actual)
            page.get_by_role('button', name='Fullscreen', exact=True).click()
            page.get_by_role('button', name='Exit fullscreen', exact=True).wait_for(state='visible')
            page.get_by_role('button', name='Exit fullscreen', exact=True).click()
            assert page.get_by_role('link', name='Snapshot').get_attribute('href') == 'frame.jpg'
            if screenshot:
                page.screenshot(path=screenshot)
            # Missed status polls must not stop a working picture. Wait longer
            # than the old five-failure limit; page waits keep route handlers running.
            page.route('**/stats', lambda route: route.abort())
            page.wait_for_timeout(7000)
            assert page.evaluate('ended') is False
            assert page.locator('#view').get_attribute('src')
            assert server.stats()['viewers'] == 1
            assert 'Waiting for the host' in page.locator('#error').inner_text()
            page.unroute('**/stats')
            page.wait_for_function('["Live", "Live · waiting for changes"].includes(document.getElementById("status").textContent)')
            # The picture kept playing, so also wait for a poll to succeed.
            page.wait_for_function('!document.getElementById("error").textContent.includes("Waiting for the host")')
            # A long outage must not present a stale picture as live. Shift the
            # last success only after a poll has failed, so none still in flight resets it.
            page.route('**/stats', lambda route: route.abort())
            page.wait_for_function('failures > 0')
            page.evaluate('lastPollOk -= 31000')
            page.get_by_role('heading', name='Can’t reach OmaBeam').wait_for()
            assert page.locator('#view').get_attribute('src') is None
            assert page.evaluate('ended') is False
            page.unroute('**/stats')
            page.wait_for_function('document.getElementById("view").naturalWidth > 0')
            page.locator('#disconnected').wait_for(state='hidden')
            eventually(lambda: server.stats()['viewers'] == 1)
            # An unknown or removed share ends at once instead of retrying.
            gone = browser.new_page()
            gone.route('**/stats', lambda route: route.fulfill(status=404, body='not found'))
            gone.goto(server.url, wait_until='domcontentloaded')
            gone.wait_for_function('document.getElementById("status").textContent === "Share ended"')
            assert gone.locator('#error').inner_text() == 'This share has ended.'
            gone.close()
            # Simulate the public diagnostics contract after source loss. Frames
            # must disappear even if the TCP stream itself has not errored yet.
            page.route('**/stats', lambda route: route.fulfill(json={**server.stats(), 'state': 'ended', 'error': 'Selected window closed'}))
            page.get_by_role('heading', name='OmaBeam disconnected').wait_for()
            assert page.locator('#disconnected').is_visible()
            assert page.locator('#disconnected .oma-logo').is_visible()
            assert page.locator('#disconnected .oma-wordmark').is_visible()
            assert not page.locator('#view').is_visible()
            assert page.locator('#status').inner_text() == 'Share ended'
            assert page.locator('#error').inner_text() == 'Selected window closed'
            assert page.locator('#view').get_attribute('src') is None
            eventually(lambda: server.stats()['viewers'] == 0)
            assert not errors, errors
        finally:
            browser.close()
    print('PASS browser: native-pixel diagnostics, viewer cleanup, live image, pause/resume, fit modes, fullscreen, missed polls, unreachable host, ended share, source-loss feedback')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', default='target/debug/omabeam')
    parser.add_argument('--browser', action='store_true')
    parser.add_argument('--browser-executable', default=os.environ.get('OMABEAM_TEST_CHROMIUM') or shutil.which('google-chrome') or shutil.which('chromium'))
    parser.add_argument('--screenshot')
    parser.add_argument('--capture-output', help='Test an output in an existing Wayland session')
    parser.add_argument('--capture-scale', type=int, help='Assert this integer output scale in capture tests')
    opts = parser.parse_args()
    if opts.capture_output:
        for source, region in [(['--live', 'output', opts.capture_output], False), (['--live', 'region', opts.capture_output, '10', '20', '160', '100'], True)]:
            for native, limit in [(False, None), (True, None), (True, 128)]:
                arguments = ['--jpeg', '--fps', '5', '--cursor']
                if native:
                    arguments.append('--native-pixels')
                if limit:
                    arguments.extend(['--width', str(limit)])
                with Server(opts.binary, arguments, source) as server:
                    snapshot = decode(server.get('frame.jpg')[1])
                    stats = server.stats()
                    d = stats['diagnostics']
                    logical = (d['logical_width'], d['logical_height'])
                    captured = (d['capture_width'], d['capture_height'])
                    if region:
                        assert logical == (160, 100), logical
                    if opts.capture_scale:
                        assert captured == tuple(value * opts.capture_scale for value in logical), d
                    expected = captured if native else logical
                    if limit and expected[0] > limit:
                        expected = (limit, max(1, expected[1] * limit // expected[0]))
                    assert snapshot.size == expected, (snapshot.size, expected)
                    assert (stats['width'], stats['height']) == expected
                    assert d['native_pixels'] == native
                    with urllib.request.urlopen(server.url + 'stream', timeout=5) as stream:
                        assert frame(stream).size == expected
                        assert frame(stream).size == expected
        print('PASS real compositor output/region capture, logical/native pixels, width caps, and diagnostics')
        return
    with Server(opts.binary, ['--jpeg', '--fps', '5', '--width', '320']) as server:
        assert server.get('')[0] == 200
        assert server.get('missing')[0] == 404
        assert decode(server.get('frame.jpg')[1]).size == (320, 180)
        with urllib.request.urlopen(server.url + 'stream', timeout=4) as stream:
            assert frame(stream).size == (320, 180)
            assert frame(stream).size == (320, 180)
            eventually(lambda: server.stats()['viewers'] == 1)
            time.sleep(2.5)
            stats = server.stats()
            assert 2 < stats['fps'] <= 5.5, stats
            d = stats['diagnostics']
            assert not d['native_pixels']
            assert (d['capture_width'], d['capture_height']) == (640, 360)
            assert d['encode_ms']['samples'] > 0 and d['encode_ms']['p95'] > 0
            assert d['capture_wait_ms']['samples'] > 0
            assert d['bytes_sent'] > 0 and d['frames_sent'] >= 2
            assert d['outgoing_mbps'] > 0
            assert len(stats['clients']) == 1
            assert stats['clients'][0]['frames_sent'] >= 2
            assert stats['clients'][0]['frame_age_ms'] >= 0
        eventually(lambda: server.stats()['viewers'] == 0)
        assert not server.stats()['clients']
        time.sleep(3)
        assert server.stats()['fps'] <= 1.5, server.stats()
        if sys.platform.startswith('linux'):
            status = subprocess.run([server.binary, '--status'], env=server.env, capture_output=True, text=True, check=True).stdout
        else:
            # CLI liveness uses Linux /proc identity checks. macOS demo builds
            # still publish the same bounded status record for serialization QA.
            status = (Path(server.runtime.name) / 'omabeam/live.json').read_text()
            print('SKIP Linux-only CLI process identity; checking persisted status instead')
        assert json.loads(status)['frames'] > 0
        assert json.loads(status)['diagnostics']['bytes_sent'] > 0
        assert len(status.encode()) <= 8192
        assert 'clients' not in json.loads(status)
        assert server.stats()['diagnostics']['outgoing_mbps'] == 0
        print('PASS real HTTP/MJPEG, dimensions, FPS, idle rate, viewer cleanup, session status')
    for width in [1, 17, 320]:
        for quality in [1, 55, 95]:
            with Server(opts.binary, ['--jpeg', '--width', str(width), '--quality', str(quality)]) as server:
                assert decode(server.get('frame.jpg')[1]).size == (width, max(1, width * 360 // 640))
    print('PASS nine JPEG decode cases')
    for arguments in [['--demo', '--fps', '0'], ['--demo', '--quality', '96'], ['--width', '0'], ['--live', 'output', 'DP-1', 'extra'], ['--demo', 'unexpected']]:
        result = subprocess.run([opts.binary, *arguments], capture_output=True, timeout=5)
        assert result.returncode == 1 and result.stderr, arguments
    print('PASS invalid arguments rejected before starting capture')
    if opts.browser:
        with Server(opts.binary, ['--jpeg', '--fps', '10', '--native-pixels']) as server:
            browser_check(server, opts.screenshot, opts.browser_executable)


if __name__ == '__main__':
    main()
