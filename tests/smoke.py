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
import subprocess
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


def browser_check(server, screenshot):
    from playwright.sync_api import sync_playwright
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        page = browser.new_page(viewport={'width': 480, 'height': 380})
        errors = []
        page.on('pageerror', lambda error: errors.append(str(error)))
        try:
            page.goto(server.url, wait_until='domcontentloaded')
            page.wait_for_function('document.getElementById("view").naturalWidth > 0')
            eventually(lambda: server.stats()['viewers'] == 1)
            page.get_by_role('button', name='Pause', exact=True).click()
            eventually(lambda: server.stats()['viewers'] == 0)
            assert page.locator('#status').inner_text() == 'Paused'
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
            # Simulate the public diagnostics contract after source loss. Frames
            # must disappear even if the TCP stream itself has not errored yet.
            page.route('**/stats', lambda route: route.fulfill(json={**server.stats(), 'state': 'ended', 'error': 'Selected window closed'}))
            page.get_by_role('heading', name='OmaBeam disconnected').wait_for()
            assert page.locator('#disconnected').is_visible()
            assert not page.locator('#view').is_visible()
            assert page.locator('#status').inner_text() == 'Share ended'
            assert page.locator('#error').inner_text() == 'Selected window closed'
            assert page.locator('#view').get_attribute('src') is None
            eventually(lambda: server.stats()['viewers'] == 0)
            assert not errors, errors
        finally:
            browser.close()
    print('PASS browser: live image, pause/resume, fit modes, fullscreen, source-loss feedback')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', default='target/debug/omabeam')
    parser.add_argument('--browser', action='store_true')
    parser.add_argument('--screenshot')
    parser.add_argument('--capture-output', help='Test an output in an existing Wayland session')
    opts = parser.parse_args()
    if opts.capture_output:
        for source, expected in [(['--live', 'output', opts.capture_output], None), (['--live', 'region', opts.capture_output, '10', '20', '160', '100'], (160, 100))]:
            with Server(opts.binary, ['--fps', '5', '--cursor'], source) as server:
                snapshot = decode(server.get('frame.jpg')[1])
                if expected:
                    assert snapshot.size == expected, snapshot.size
                with urllib.request.urlopen(server.url + 'stream', timeout=5) as stream:
                    assert frame(stream).size == snapshot.size
                    assert frame(stream).size == snapshot.size
        print('PASS real compositor output and region JPEG/MJPEG with cursor enabled')
        return
    with Server(opts.binary, ['--fps', '5', '--width', '320']) as server:
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
        eventually(lambda: server.stats()['viewers'] == 0)
        time.sleep(3)
        assert server.stats()['fps'] <= 1.5, server.stats()
        status = subprocess.run([server.binary, '--status'], env=server.env, capture_output=True, text=True, check=True)
        assert json.loads(status.stdout)['frames'] > 0
        print('PASS real HTTP/MJPEG, dimensions, FPS, idle rate, viewer cleanup, CLI status')
    for width in [1, 17, 320]:
        for quality in [1, 55, 95]:
            with Server(opts.binary, ['--width', str(width), '--quality', str(quality)]) as server:
                assert decode(server.get('frame.jpg')[1]).size == (width, max(1, width * 360 // 640))
    print('PASS nine JPEG decode cases')
    for arguments in [['--demo', '--fps', '0'], ['--demo', '--quality', '96'], ['--width', '0'], ['--live', 'output', 'DP-1', 'extra'], ['--demo', 'unexpected']]:
        result = subprocess.run([opts.binary, *arguments], capture_output=True, timeout=5)
        assert result.returncode == 1 and result.stderr, arguments
    print('PASS invalid arguments rejected before starting capture')
    if opts.browser:
        with Server(opts.binary, ['--fps', '10']) as server:
            browser_check(server, opts.screenshot)


if __name__ == '__main__':
    main()
