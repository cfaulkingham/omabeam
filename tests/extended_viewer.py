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
        self.temp = tempfile.TemporaryDirectory(prefix='ob-client-', dir='/tmp/opencode' if Path('/tmp/opencode').is_dir() else None)
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


def watch_overlay(page):
    # Class records keep their old value, so a blocked/unreachable state that
    # appears and clears within one task is still recorded.
    page.evaluate("""() => {
        window.overlaySeen = false;
        const shown = value => /\\b(blocked|unreachable)\\b/.test(value || '');
        new MutationObserver(records => {
            overlaySeen ||= shown(stage.className) || records.some(record => shown(record.oldValue));
        }).observe(stage, {attributes: true, attributeFilter: ['class'], attributeOldValue: true});
    }""")


def leave_fullscreen(page):
    # Chrome can retain user activation across refresh/resume, allowing the
    # viewer's automatic fullscreen request. Header controls are then covered.
    if page.evaluate("document.fullscreenElement === stage || stage.classList.contains('expanded')"):
        page.mouse.move(20, 20)
        page.locator('#exit').click()
        page.wait_for_function("document.fullscreenElement === null && !stage.classList.contains('expanded')")


def check_setup(browser, artifacts):
    """Deterministic first-visit states, without changing a real desktop or share."""
    html = Path('src/live/viewer.html').read_text()
    desktop = dict(config=dict(width=1280, height=720, scale=1), updating=False, error=None)
    stats = dict(state='live', width=1280, height=720, fps=30.0, viewers=1,
                 source='Test share', error='Capture warning')
    claims, requests, errors, heartbeats, restores, sized = [], [], [], [], [], []
    context = browser.new_context(viewport={'width': 320, 'height': 640})

    def respond(route):
        path = route.request.url.split('/')[-1].split('?')[0]
        requests.append(path)
        if path == 'size':
            sized.append(json.loads(route.request.post_data)['size'])  # None restores the host size.
        if route.request.url.endswith('/'):
            route.fulfill(content_type='text/html', body=html)
        elif path == 'stats':
            route.fulfill(json=stats)
        elif path == 'claim':
            claims.append(route)  # Explicitly hold the lease response.
        elif path == 'heartbeat' and heartbeats:
            route.fulfill(status=heartbeats.pop(0))
        elif path == 'size' and restores and json.loads(route.request.post_data)['size'] is None:
            route.fulfill(status=restores.pop(0))  # Only a host-size restore.
        elif path in ('heartbeat', 'size', 'release'):
            route.fulfill(json=desktop)
        elif path == 'stream':
            route.fulfill(content_type='image/svg+xml', body='<svg xmlns="http://www.w3.org/2000/svg" width="1280" height="720"/>')
        else:
            route.fulfill(status=204)

    context.route('http://viewer.test/**', respond)
    page = context.new_page()
    page.on('pageerror', lambda error: errors.append(str(error)))
    # Simulate a browser rejecting fullscreen. Regular shares never request it.
    # An owned extended display requests it immediately; denial stays windowed.
    page.add_init_script("""
        window.fullscreenCalls = 0;
        Element.prototype.requestFullscreen = function() {
            window.fullscreenCalls++;
            return Promise.reject(new Error('Fullscreen denied'));
        };
    """)
    page.goto('http://viewer.test/regular/')
    page.wait_for_function("playback === 'jpeg'")
    assert page.locator('#viewer-setup').count() == 0
    assert page.get_by_role('button', name='Keep watching', exact=True).count() == 0
    assert page.get_by_role('button', name='Fullscreen', exact=True).count() == 1
    assert page.locator('#match-device').is_hidden()
    assert page.locator('#error').inner_text() == 'Capture warning'
    assert page.locator('#error').is_visible()
    assert 'claim' not in requests and 'size' not in requests
    assert page.evaluate('fullscreenCalls') == 0
    assert page.evaluate('document.documentElement.scrollWidth <= innerWidth')
    for selector in ['#fullscreen', '#pause', '#error']:
        box = page.locator(selector).bounding_box()
        assert box['x'] >= 0 and box['x'] + box['width'] <= 320, box
    page.screenshot(path=str(artifacts / 'first-visit-mobile.png'), full_page=True)
    # Toolbar fullscreen is the only fullscreen control. A rejected Fullscreen
    # API still uses the expanded-stage fallback, including from the keyboard.
    page.locator('#fullscreen').focus()
    page.keyboard.press('Enter')
    page.wait_for_function("stage.classList.contains('expanded')")
    assert page.evaluate('fullscreenCalls') == 1
    leave_fullscreen(page)
    page.reload()
    page.wait_for_function('latestStats !== null')
    assert page.evaluate('fullscreenCalls') == 0  # Regular sharing never auto-fullscreens.

    stats['desktop'] = desktop
    page.goto('http://viewer.test/desktop/')
    eventually(lambda: page.wait_for_timeout(50) or bool(claims))
    assert page.locator('#match-device').is_disabled()
    assert page.evaluate('fullscreenCalls') == 0
    claims.pop(0).fulfill(status=409)
    page.locator('#stage.blocked').wait_for()
    assert page.evaluate('fullscreenCalls') == 0
    eventually(lambda: page.wait_for_timeout(50) or bool(claims))
    claims.pop(0).fulfill(json=desktop)
    page.wait_for_function('ownsDesktop && playback === "jpeg"')
    assert page.locator('#match-device').is_enabled()
    assert page.evaluate('fullscreenCalls') == 1
    assert not page.evaluate("document.fullscreenElement || stage.classList.contains('expanded')")
    assert page.evaluate('document.documentElement.scrollWidth <= innerWidth')
    page.screenshot(path=str(artifacts / 'first-visit-desktop-mobile.png'), full_page=True)
    # 412: this page's own lease lapsed and nobody else took the display. The
    # page claims it again and restarts media without the in-use overlay.
    watch_overlay(page)
    start = len(requests)
    heartbeats.append(412)
    eventually(lambda: page.wait_for_timeout(50) or bool(claims))
    after = requests[start:]
    assert 'heartbeat' in after and after.index('heartbeat') < after.index('claim'), after
    assert page.evaluate('!ownsDesktop && !desktopBlocked && !overlaySeen')
    claimed = len(requests)
    claims.pop(0).fulfill(json=desktop)
    page.wait_for_function("ownsDesktop && playback === 'jpeg'")
    eventually(lambda: page.wait_for_timeout(50) or 'stream' in requests[claimed:])
    assert not page.evaluate('overlaySeen || desktopBlocked')
    # A heartbeat conflict blocks the page immediately.
    page.evaluate('showDesktopBlocked()')
    assert 'blocked' in (page.locator('#stage').get_attribute('class') or '')
    assert page.locator('#match-device').is_disabled()
    # End while waiting for reacquisition.
    stats['state'] = 'ended'
    for claim in claims:
        claim.fulfill(status=409)
    claims.clear()
    page.wait_for_function('ended')
    assert page.locator('#match-device').is_disabled()
    page.goto('http://viewer.test/already-ended/')
    page.wait_for_function('ended')
    assert not claims

    # 412 on a size request: drop the lapsed lease quietly, claim it again, then
    # retry the owed size. Turning matching off (a restore) is the case that
    # the claim path alone would not resend.
    stats['state'] = 'live'
    page.goto('http://viewer.test/desktop-size/')
    eventually(lambda: page.wait_for_timeout(50) or bool(claims))
    claims.pop(0).fulfill(json=desktop)
    page.locator('#match-device').click()
    page.wait_for_function("matchEnabled && lastSizeRequest !== null && !resizing")
    watch_overlay(page)
    restores.append(412)
    page.locator('#match-device').click()
    eventually(lambda: page.wait_for_timeout(50) or bool(claims))
    assert not restores
    assert page.evaluate('!matchEnabled && !ownsDesktop && !desktopBlocked && !overlaySeen')
    claimed = len(requests)
    claims.pop(0).fulfill(json=desktop)
    page.wait_for_function("ownsDesktop && lastSizeRequest === 'null'")
    assert 'size' in requests[claimed:]
    assert not page.evaluate('overlaySeen || desktopBlocked')
    # Turning matching off owes the host a restore. A resize inside the 800 ms
    # debounce reschedules it; it must not cancel it.
    page.locator('#match-device').click()
    page.wait_for_function("matchEnabled && lastSizeRequest?.startsWith('{') && !resizing && !resizeOwed")
    before = len(sized)
    page.evaluate("() => { matchDevice.click(); dispatchEvent(new Event('resize')); }")
    eventually(lambda: page.wait_for_timeout(50) or None in sized[before:])
    page.wait_for_function("!matchEnabled && lastSizeRequest === 'null' && !resizeOwed")

    # Storage may be denied in embedded/private contexts. The page still loads,
    # and the no-API fullscreen fallback is unchanged.
    stats.pop('desktop')
    stats['state'] = 'live'
    page.add_init_script("""
        Object.defineProperty(window, 'sessionStorage', {get() { throw new Error('Storage denied'); }});
        Element.prototype.requestFullscreen = undefined;
    """)
    page.goto('http://viewer.test/no-storage/')
    page.wait_for_function("playback === 'jpeg'")
    page.locator('#fullscreen').click()
    page.wait_for_function("stage.classList.contains('expanded')")
    leave_fullscreen(page)
    assert not errors, errors
    context.close()
    print('PASS viewer setup: lease gating, blocked/ended, lapsed-lease reclaim (heartbeat and size), restore survives a resize, regular sharing, mobile, keyboard, fullscreen fallbacks, unavailable storage, visible errors')


def check_browser(fixture, browser, artifacts):
    errors = []
    first_context = browser.new_context(viewport={'width': 1000, 'height': 800}, device_scale_factor=1)
    second_context = browser.new_context(viewport={'width': 900, 'height': 700}, device_scale_factor=2)
    first = first_context.new_page()
    first.on('pageerror', lambda error: errors.append(str(error)))
    first.goto(fixture.url, wait_until='domcontentloaded')
    first.wait_for_function("playback === 'webrtc' && video.videoWidth === 1280", timeout=20000)
    assert first.locator('#match-device').is_enabled()
    assert first.locator('#viewer-setup').count() == 0
    first.mouse.move(20, 20)  # Reveal fullscreen chrome if the browser granted it on connect.
    first.screenshot(path=str(artifacts / 'first-visit-desktop.png'))
    second = second_context.new_page()
    second.on('pageerror', lambda error: errors.append(str(error)))
    second.goto(fixture.url, wait_until='domcontentloaded')
    second.get_by_role('heading', name='This display is already connected to another device.').wait_for()
    assert second.evaluate("pc === null && !img.hasAttribute('src')")
    assert second.locator('#match-device').is_disabled()
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
    assert first.locator('#match-device').get_attribute('aria-pressed') == 'true'
    assert first.locator('#match-device').evaluate('(element) => element === document.activeElement')
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

    # An outage longer than the lease grace but shorter than the viewer's
    # unreachable limit. The host answers 412 for the page's own lapsed lease,
    # not "another device"; the page claims it again and resumes H.264.
    second.wait_for_function("ownsDesktop && playback === 'webrtc' && peerId !== null")
    watch_overlay(second)
    previous_peer = second.evaluate('peerId')
    replies = []
    second.on('response', lambda response: replies.append((response.url.rsplit('/', 1)[-1], response.status))
              if '/desktop/' in response.url else None)
    second.route('**/*', lambda route: route.abort())
    eventually(lambda: second.wait_for_timeout(100) or not fixture.stats()['desktop']['occupied'], timeout=25)
    second.unroute('**/*')
    second.wait_for_function(f"ownsDesktop && playback === 'webrtc' && peerId !== null && peerId !== {json.dumps(previous_peer)}",
                             timeout=25000)
    assert ('heartbeat', 412) in replies, replies
    assert replies[replies.index(('heartbeat', 412)) + 1] == ('claim', 200), replies
    assert not second.evaluate('overlaySeen || desktopBlocked || unreachable')
    assert fixture.stats()['desktop']['occupied']
    second_context.close()
    first_context.close()
    assert not errors, errors
    physical = fixture.compositor.outputs['DP-1']
    assert (physical['width'], physical['height'], physical['x'], physical['y'], physical['scale']) == (1920, 1080, 0, 0, 1)
    print('PASS extended viewer: exclusive ownership, duplicate tabs, blocked media, refresh, transport switching, pause/resume, resize, rollback, host-size restore, disconnect grace, HiDPI, portrait, fullscreen, lapsed-lease reclaim after an outage, cleanup')


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
                check_setup(browser, args.artifacts)
                check_browser(fixture, browser, args.artifacts)
            finally:
                browser.close()


if __name__ == '__main__':
    main()
