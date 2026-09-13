#!/usr/bin/env python3
"""Live Hyprland acceptance: creates and removes one temporary extra display.

Run from a Hyprland terminal with no active OmaBeam share. Requires Pillow;
--browser also uses Playwright Chromium. Never stops a pre-existing share.
"""
import argparse
import io
import json
import os
from pathlib import Path
import subprocess
import time
import urllib.request
from PIL import Image


def wait_for(check, timeout=20):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            value = check()
            if value:
                return value
        except (OSError, ValueError, AssertionError) as error:
            last = error
        time.sleep(.1)
    raise AssertionError(f'acceptance check timed out: {last}')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', default='target/debug/omabeam')
    parser.add_argument('--browser', action='store_true')
    parser.add_argument('--artifacts', default='target/extended-review/live')
    args = parser.parse_args()
    binary = str(Path(args.binary).resolve())
    artifacts = Path(args.artifacts).resolve()
    artifacts.mkdir(parents=True, exist_ok=True)

    def command(*arguments):
        return subprocess.run([binary, *arguments], capture_output=True, text=True, check=True)

    current = subprocess.run([binary, '--status'], capture_output=True, text=True)
    assert current.returncode == 1, 'stop the existing share (or resolve its status error) before this test'
    baseline = json.loads(command('--hypr', 'monitors').stdout)
    assert baseline, 'at least one active display is required'
    baseline_layout = {m['name']: (m['width'], m['height'], m['x'], m['y'], m['scale']) for m in baseline}
    runtime = Path(os.environ['XDG_RUNTIME_DIR']) / 'omabeam'
    owned_name = None
    process = None
    log = (artifacts / 'session.log').open('w')
    try:
        process = subprocess.Popen([binary, '--bind', '127.0.0.1', '--port', '0', '--native-pixels', '--cursor',
            '--live', 'extend', '1280', '720', '1', 'right'], stdout=log, stderr=log)

        def ready():
            assert process.poll() is None, (artifacts / 'session.log').read_text()
            status = json.loads((runtime / 'live.json').read_text())
            return status if status['pid'] == process.pid and status['state'] == 'live' else None

        status = wait_for(ready)
        owned_name = json.loads((runtime / 'display.json').read_text())['name']
        monitors = json.loads(command('--hypr', 'monitors').stdout)
        new = [m for m in monitors if m['name'] == owned_name]
        assert len(new) == 1 and (new[0]['width'], new[0]['height']) == (1280, 720), monitors
        assert {m['name']: (m['width'], m['height'], m['x'], m['y'], m['scale']) for m in monitors if m['name'] != owned_name} == baseline_layout
        (artifacts / 'monitors.json').write_text(json.dumps(monitors, indent=2))
        with urllib.request.urlopen(status['url'] + 'frame.jpg', timeout=10) as response:
            jpeg = response.read()
        frame = Image.open(io.BytesIO(jpeg))
        assert frame.size == (1280, 720), frame.size
        frame.save(artifacts / 'extended-display.png')

        if args.browser:
            from playwright.sync_api import sync_playwright
            with sync_playwright() as playwright:
                browser = playwright.chromium.launch(headless=True)
                page = browser.new_page()
                page.goto(status['url'])
                page.wait_for_function("document.querySelector('img').naturalWidth === 1280")
                page.wait_for_function("document.querySelector('#source').textContent.includes('Extended desktop')")
                page.screenshot(path=str(artifacts / 'viewer.png'))
                browser.close()
            # Disconnecting the viewer must leave the desktop available.
            assert any(m['name'] == owned_name for m in json.loads(command('--hypr', 'monitors').stdout))

        command('--stop')
        assert process.wait(15) == 0, (artifacts / 'session.log').read_text()
        after = json.loads(command('--hypr', 'monitors').stdout)
        assert {m['name']: (m['width'], m['height'], m['x'], m['y'], m['scale']) for m in after} == baseline_layout
        assert not (runtime / 'display.json').exists()
        print('PASS extended desktop: real output, unchanged physical layout, captured pixels, graceful stop and removal')
    finally:
        if process is not None and process.poll() is None:
            process.terminate()
            try:
                process.wait(15)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        # Only retry cleanup if the remaining journal belongs to this test.
        if owned_name and (runtime / 'display.json').exists():
            saved = json.loads((runtime / 'display.json').read_text())
            if saved['name'] == owned_name:
                command('--stop')
        log.close()


if __name__ == '__main__':
    main()
