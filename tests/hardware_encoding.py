#!/usr/bin/env python3
"""Encoder selection and hardware acceptance using generated frames only.

Default checks need no GPU. --require-hardware also requires the real helper
to encode on this machine's GPU at every checked size; failures never count
as hardware success.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import tempfile


DEFAULT_SIZES = [(640, 360), (1920, 1080), (3840, 2160)]


def probe(binary, mode, success=True, sizes=()):
    # In auto mode each size may wait out the helper's 5 s start timeout and a
    # 0.75 s frame timeout before falling back to software: allow 10 s a size.
    timeout = 5 + 10 * len(sizes or DEFAULT_SIZES)
    result = subprocess.run([str(binary), '--check-encoders', *sizes, '--encoder', mode],
                            capture_output=True, text=True, timeout=timeout)
    assert (result.returncode == 0) == success, result.stdout + result.stderr
    if not success:
        return result.stderr
    report = json.loads(result.stdout)
    # One result per distinct size, rounded up to even as a stream would be.
    requested = dict.fromkeys(tuple(map(int, re.split('[xX×]', size))) for size in sizes)
    expected = [(width + width % 2, height + height % 2) for width, height in requested] or DEFAULT_SIZES
    assert [(size['width'], size['height']) for size in report['sizes']] == expected, report
    # The top-level fields are the first size without hardware, else the first size.
    summary = next((size for size in report['sizes'] if not size['hardware']), report['sizes'][0])
    assert {key: report[key] for key in summary} == summary, report
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', default='target/debug/omabeam', type=Path)
    parser.add_argument('--require-hardware', action='store_true')
    parser.add_argument('--browser', action='store_true', help='Require hardware playback and test recovery after killing its helper')
    parser.add_argument('--browser-executable')
    parser.add_argument('--screenshot', type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    # An isolated copy has no adjacent helper or FFmpeg runtime requirement.
    with tempfile.TemporaryDirectory(prefix='omabeam-encoder-') as root:
        isolated = Path(root) / 'omabeam'
        shutil.copy2(binary, isolated)
        auto = probe(isolated, 'auto')
        assert auto['encoder'] == 'OpenH264 software' and not auto['hardware'] and auto['note'], auto
        assert all(size['encoder'] == 'OpenH264 software' and not size['hardware'] and size['note']
                   for size in auto['sizes']), auto
        software = probe(isolated, 'software')
        assert not software['hardware'] and software['note'] is None, software
        assert all(not size['hardware'] and size['note'] is None for size in software['sizes']), software
        probe(isolated, 'software', sizes=['1280x720', '1279X719', '720×1280', '1280x720'])
        assert 'helper' in probe(isolated, 'hardware', success=False)
        assert 'WxH' in probe(isolated, 'software', success=False, sizes=['1920'])
        assert '3840' in probe(isolated, 'software', success=False, sizes=['4096x2160'])
    actual = probe(binary, 'auto')
    if args.require_hardware:
        assert all(size['hardware'] for size in actual['sizes']), actual
        assert all(size['hardware'] for size in probe(binary, 'hardware')['sizes'])
    print('PASS encoder selection at 640x360, 1080p, 4K, and requested sizes: missing helper fallback, '
          'explicit software, required hardware, and real local probe')
    print(json.dumps(actual, indent=2))
    if args.browser:
        from playwright.sync_api import sync_playwright
        from smoke import Server, eventually
        from webrtc import playing
        with Server(binary, ['--webrtc', '--webrtc-port', '0']) as server:
            with sync_playwright() as playwright:
                browser = playwright.chromium.launch(headless=True, executable_path=args.browser_executable)
                try:
                    page = browser.new_page(viewport={'width': 390, 'height': 650})
                    page.goto(server.url)
                    playing(page)
                    selected = server.stats()['webrtc']['encoder']
                    assert selected != 'OpenH264 software', server.stats()['webrtc']
                    children = subprocess.check_output(['pgrep', '-P', str(server.proc.pid)], text=True).split()
                    helpers = [int(pid) for pid in children if Path(subprocess.check_output(
                        ['ps', '-p', pid, '-o', 'comm='], text=True).strip()).name == 'omabeam-encoder']
                    assert len(helpers) == 1, helpers
                    before = page.evaluate('browserStats.frames')
                    os.kill(helpers[0], signal.SIGKILL)
                    eventually(lambda: server.stats()['webrtc']['encoder'] == 'OpenH264 software')
                    page.wait_for_function('(before) => playback === "webrtc" && browserStats.frames > before + 3', arg=before)
                    assert server.stats()['webrtc']['encoder_note']
                    page.locator('#diagnostics summary').click()
                    page.wait_for_function('!document.querySelector("#rtc-note-row").hidden')
                    assert page.evaluate('document.documentElement.scrollWidth <= innerWidth')
                    if args.screenshot:
                        page.screenshot(path=str(args.screenshot))
                    print(f'PASS hardware browser recovery: {selected} to OpenH264 after helper termination; decoded video continues')
                finally:
                    browser.close()


if __name__ == '__main__':
    main()
