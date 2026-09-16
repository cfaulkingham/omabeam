#!/usr/bin/env python3
"""Extended-display browser acceptance against real Rust media/IPC and synthetic pixels.

Uses a private simulated compositor, never the host's desktop. --serve keeps
the fixture available for manual browser review until its stop file is created.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import urllib.error
import urllib.request

from extended_desktop import Compositor


def eventually(check, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = check()
        if result:
            return result
        time.sleep(.1)
    raise AssertionError('acceptance check timed out')


class Fixture:
    def __enter__(self):
        self.temp = tempfile.TemporaryDirectory(prefix='ob-client-', dir='/tmp')
        self.root = Path(self.temp.name)
        self.compositor = Compositor(self.root)
        self.log = (self.root / 'fixture.log').open('w')
        env = {**os.environ, 'XDG_RUNTIME_DIR': str(self.root),
               'HYPRLAND_INSTANCE_SIGNATURE': 'fixture', 'OMABEAM_FIXTURE_DIR': str(self.root)}
        self.process = subprocess.Popen(['cargo', 'test', '-p', 'omabeam', '--lib', '--locked',
            'extended_desktop_browser_fixture', '--', '--ignored'], env=env, stdout=self.log, stderr=self.log)
        try:
            def ready():
                assert self.process.poll() is None, (self.root / 'fixture.log').read_text()
                path = self.root / 'ready.json'
                if path.exists():
                    self.url = json.loads(path.read_text())['url']
                    return True
            eventually(ready, timeout=60)
            return self
        except BaseException:
            self.__exit__(None, None, None)
            raise

    def __exit__(self, *_):
        (self.root / 'stop').touch()
        try:
            self.process.wait(15)
        except subprocess.TimeoutExpired:
            self.process.terminate()
            self.process.wait(5)
        self.log.close()
        try:
            assert self.process.returncode == 0, (self.root / 'fixture.log').read_text()
            assert list(self.compositor.outputs) == ['DP-1'], self.compositor.outputs
            assert not (self.root / 'omabeam/display.json').exists()
        finally:
            self.compositor.close()
            self.temp.cleanup()

    def get(self, path):
        try:
            response = urllib.request.urlopen(self.url + path, timeout=5)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            return response.status, response.read()

    def stats(self):
        code, body = self.get('stats')
        assert code == 200
        return json.loads(body)


def leave_fullscreen(page):
    # Chrome can retain user activation across refresh/resume, allowing the
    # viewer's automatic fullscreen request. Header controls are then covered.
    if page.evaluate("document.fullscreenElement === stage || stage.classList.contains('expanded')"):
        page.mouse.move(20, 20)
        page.locator('#exit').click()
        page.wait_for_function("document.fullscreenElement === null && !stage.classList.contains('expanded')")


def check_browser(fixture, browser, artifacts):
    errors = []
    first_context = browser.new_context(viewport={'width': 1000, 'height': 800}, device_scale_factor=1)
    second_context = browser.new_context(viewport={'width': 900, 'height': 700}, device_scale_factor=2)
    first = first_context.new_page()
    first.on('pageerror', lambda error: errors.append(str(error)))
    first.goto(fixture.url, wait_until='domcontentloaded')
    first.wait_for_function("playback === 'webrtc' && video.videoWidth === 1280", timeout=20000)
    leave_fullscreen(first)
    second = second_context.new_page()
    second.on('pageerror', lambda error: errors.append(str(error)))
    second.goto(fixture.url, wait_until='domcontentloaded')
    second.get_by_role('heading', name='This display is already connected to another device.').wait_for()
    assert second.evaluate("pc === null && !img.hasAttribute('src')")
    for path in ['frame.jpg', 'stream', 'webrtc/offer', 'frame.jpg?viewer=' + '0' * 32]:
        assert fixture.get(path)[0] == 409, path
    assert fixture.stats()['webrtc']['peers'] == 1
    second.screenshot(path=str(artifacts / 'display-in-use.png'))

    # A tab duplicated with sessionStorage copied must not displace an active page.
    with first.expect_popup() as popup:
        first.evaluate("window.open(location.href, '_blank')")
    cloned = popup.value
    cloned.get_by_role('heading', name='This display is already connected to another device.').wait_for()
    cloned.close()

    first.locator('#match-device').click()
    first.wait_for_function("latestStats.desktop.matched && !latestStats.desktop.updating && video.videoWidth === 1000")
    first.wait_for_function("video.videoHeight === Math.floor(stage.getBoundingClientRect().height / 2) * 2")
    first.set_viewport_size({'width': 1180, 'height': 860})
    first.wait_for_function("video.videoWidth === 1180")
    first.screenshot(path=str(artifacts / 'matched-desktop.png'))
    first.reload(wait_until='domcontentloaded')
    first.wait_for_function("ownsDesktop && playback === 'webrtc' && video.videoWidth === 1180", timeout=20000)
    leave_fullscreen(first)
    assert second.locator('#stage').get_attribute('class').find('blocked') >= 0

    # JPEG uses the same ownership and tracks both dimensions when resizing.
    first.locator('#transport').click()
    first.wait_for_function("playback === 'jpeg' && img.naturalWidth === 1180")
    first.set_viewport_size({'width': 1080, 'height': 850})
    first.wait_for_function("img.naturalWidth === 1080")
    assert fixture.get('frame.jpg')[0] == 409
    first.locator('#transport').click()
    first.wait_for_function("playback === 'webrtc' && video.videoWidth === 1080", timeout=20000)
    before_fallback = first.locator('#snapshot').get_attribute('href')
    first.evaluate('pc.close()')
    first.wait_for_function("ownsDesktop && playback === 'jpeg' && img.naturalWidth === 1080")
    assert first.locator('#snapshot').get_attribute('href') == before_fallback
    assert first.locator('#transport').inner_text() == 'Video: Auto'
    first.locator('#transport').click()
    first.wait_for_function("playback === 'jpeg' && preferRtc === false")
    first.locator('#transport').click()
    first.wait_for_function("ownsDesktop && playback === 'webrtc'", timeout=20000)
    first.locator('#pause').click()
    eventually(lambda: fixture.stats()['viewers'] == 0)
    first.locator('#pause').click()
    first.wait_for_function("ownsDesktop && playback === 'webrtc'", timeout=20000)
    leave_fullscreen(first)

    # A rejected IPC mode leaves the previous capture alive and reports the failure.
    previous = fixture.stats()['desktop']['config']
    fixture.compositor.fail_next_config = True
    first.set_viewport_size({'width': 1120, 'height': 850})
    first.wait_for_function("latestStats.desktop.error !== null")
    assert fixture.stats()['desktop']['config'] == previous
    assert fixture.stats()['state'] == 'live'
    first.set_viewport_size({'width': 1140, 'height': 850})
    first.wait_for_function("video.videoWidth === 1140 && latestStats.desktop.error === null")
    first.locator('#match-device').click()
    first.wait_for_function("!latestStats.desktop.matched && video.videoWidth === 1280 && video.videoHeight === 720")

    # Close reserves the original client's place, then another device can connect.
    first.close()
    assert fixture.stats()['desktop']['occupied']
    assert len(fixture.compositor.outputs) == 2
    second.wait_for_function("ownsDesktop && playback === 'webrtc'", timeout=25000)
    second.locator('#stage').click(position={'x': 40, 'y': 40})
    second.wait_for_function("document.fullscreenElement === stage || stage.classList.contains('expanded')")
    second.locator('#exit').click()
    second.wait_for_function("document.fullscreenElement === null && !stage.classList.contains('expanded')")
    second.locator('#match-device').click()
    second.wait_for_function("latestStats.desktop.matched && video.videoWidth === 1800")
    assert fixture.stats()['desktop']['config']['scale'] == 2
    second.screenshot(path=str(artifacts / 'hidpi-desktop.png'))
    # Portrait requests and fullscreen changes go through the same size path.
    second.set_viewport_size({'width': 700, 'height': 1000})
    second.wait_for_function("video.videoWidth === 1400 && video.videoHeight > video.videoWidth")
    second.locator('#fullscreen').click()
    second.wait_for_function("video.videoHeight === Math.floor(stage.getBoundingClientRect().height * 2 / 2) * 2")
    second.locator('#exit').click()
    second.wait_for_function("video.videoHeight === Math.floor(stage.getBoundingClientRect().height * 2 / 2) * 2")
    second_context.close()
    first_context.close()
    assert not errors, errors
    physical = fixture.compositor.outputs['DP-1']
    assert (physical['width'], physical['height'], physical['x'], physical['y'], physical['scale']) == (1920, 1080, 0, 0, 1)
    print('PASS extended viewer: exclusive ownership, duplicate tabs, blocked media, refresh, transport switching, pause/resume, resize, rollback, host-size restore, disconnect grace, HiDPI, portrait, fullscreen, cleanup')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--serve', action='store_true')
    parser.add_argument('--browser-executable', default=os.environ.get('OMABEAM_TEST_CHROMIUM'))
    parser.add_argument('--artifacts', type=Path, default=Path('target/extended-review/client'))
    args = parser.parse_args()
    with Fixture() as fixture:
        if args.serve:
            print(json.dumps({'url': fixture.url, 'stop': str(fixture.root / 'stop')}), flush=True)
            while fixture.process.poll() is None and not (fixture.root / 'stop').exists():
                time.sleep(.2)
            return
        args.artifacts.mkdir(parents=True, exist_ok=True)
        from playwright.sync_api import sync_playwright
        with sync_playwright() as playwright:
            browser = playwright.chromium.launch(headless=True, executable_path=args.browser_executable)
            try:
                check_browser(fixture, browser, args.artifacts)
            finally:
                browser.close()


if __name__ == '__main__':
    main()
