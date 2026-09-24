#!/usr/bin/env python3
"""Real Chromium H.264 decoding, LAN HTTP, fallback and peer lifecycle checks."""
import argparse
import json
import os
import shutil
import socket
import subprocess
import time
import urllib.error
import urllib.parse
import urllib.request
from pathlib import Path
from playwright.sync_api import sync_playwright
from smoke import Server, decode, eventually


def post(server, path, value, headers=None):
    body = json.dumps(value).encode()
    request = urllib.request.Request(urllib.parse.urljoin(server.url, path), body, {'Content-Type': 'application/json', **(headers or {})})
    try:
        response = urllib.request.urlopen(request, timeout=5)
    except urllib.error.HTTPError as error:
        response = error
    with response:
        return response.status, response.read()


def playing(page):
    page.wait_for_function("playback === 'webrtc' && video.videoWidth > 0", timeout=15000)
    page.wait_for_function("browserStats?.frames > 2", timeout=10000)
    assert page.evaluate("browserStats.codec") == 'video/H264'
    # Inspect decoded pixels, not only video dimensions or an ICE state.
    pixels = page.evaluate("""() => {
        const canvas = document.createElement('canvas'); canvas.width = video.videoWidth; canvas.height = video.videoHeight;
        const c = canvas.getContext('2d'); c.drawImage(video, 0, 0);
        const data = c.getImageData(0, 0, canvas.width, canvas.height).data;
        return [...new Set(Array.from(data).filter((_, i) => i % 4 !== 3))].length;
    }""")
    assert pixels > 3, pixels


def latency_controls(page):
    page.evaluate("""async () => {
        const absent = {};
        preferLowLatency(absent);
        if ('jitterBufferTarget' in absent) throw new Error('created unsupported property');
        preferLowLatency({get jitterBufferTarget() { return null; }, set jitterBufferTarget(v) { throw new Error('unsupported'); }});
        const receiver = pc.getReceivers().find(r => r.track.kind === 'video');
        if ('jitterBufferTarget' in receiver && receiver.jitterBufferTarget !== 0)
            throw new Error('low-latency target was not requested');
        const connection = pc, getStats = connection.getStats;
        const savedPrevious = previousInbound, savedStats = browserStats;
        try {
            previousInbound = null;
            let inbound = {id: 'latency-fixture', type: 'inbound-rtp', kind: 'video', timestamp: 1000,
                framesDecoded: 100, bytesReceived: 1000, packetsLost: 0, jitter: 0,
                totalDecodeTime: 5, jitterBufferDelay: 20, jitterBufferEmittedCount: 100};
            connection.getStats = async () => new Map([['inbound', inbound]]);
            await sampleBrowser();
            if (browserStats.decode !== null || browserStats.buffer !== null)
                throw new Error('first sample used lifetime averages');
            inbound = {...inbound, timestamp: 2000, framesDecoded: 104, bytesReceived: 2000,
                totalDecodeTime: 5.008, jitterBufferDelay: 20.04, jitterBufferEmittedCount: 104};
            await sampleBrowser();
            if (Math.abs(browserStats.decode - 2) > 0.001 || Math.abs(browserStats.buffer - 10) > 0.001)
                throw new Error('recent timing did not use interval deltas');
            await sampleBrowser();
            if (browserStats.decode !== null || browserStats.buffer !== null)
                throw new Error('idle interval should have no timing samples');
            inbound = {...inbound, id: 'replacement-stream', framesDecoded: 1};
            await sampleBrowser();
            if (browserStats.decode !== null || browserStats.buffer !== null || browserStats.fps !== null)
                throw new Error('new stream reused previous counters');
        } finally {
            connection.getStats = getStats;
            previousInbound = savedPrevious;
            browserStats = savedStats;
        }
    }""")


def checks(server, browser, screenshot):
    page = browser.new_page(viewport={'width': 1280, 'height': 900})
    errors = []
    page.on('pageerror', lambda error: errors.append(str(error)))
    page.goto(server.url, wait_until='domcontentloaded')
    playing(page)
    latency_controls(page)
    assert not page.evaluate('isSecureContext'), 'Use the LAN HTTP origin to avoid localhost-only WebRTC success'
    eventually(lambda: server.stats()['webrtc']['connected'] == 1)
    assert server.stats()['clients'] == []
    for stage in ['capture_to_encode_ms', 'convert_ms', 'codec_ms', 'send_queue_ms']:
        eventually(lambda: server.stats()['webrtc'][stage]['samples'] > 0)
        assert server.stats()['webrtc'][stage]['p95'] >= 0
    assert server.stats()['diagnostics']['encode_ms']['samples'] == 0, 'RTC-only playback should not encode JPEG'
    page.locator('#fit').click()
    assert round(page.locator('#video').bounding_box()['width']) == 1280
    page.locator('#fit').click()
    assert round(page.locator('#video').bounding_box()['width']) == 640
    page.locator('#fit').click()
    page.locator('#fullscreen').click()
    page.locator('#exit').wait_for(state='visible')
    page.locator('#exit').click()
    page.locator('#diagnostics summary').click()
    page.wait_for_function("document.querySelector('#rtc-codec').textContent.includes('video/H264')")
    for element in ['rtc-wait', 'rtc-convert', 'rtc-codec-time', 'rtc-queue']:
        page.wait_for_function('(id) => document.getElementById(id).textContent.includes("ms")', arg=element)
    assert page.locator('#rtc-help').is_visible()
    if screenshot:
        page.screenshot(path=str(screenshot))
        page.set_viewport_size({'width': 390, 'height': 650})
        assert page.evaluate('document.documentElement.scrollWidth <= innerWidth')
        page.screenshot(path=str(screenshot.with_name('webrtc-mobile.png')))
        page.set_viewport_size({'width': 1280, 'height': 900})
    code, jpeg = server.get('frame.jpg')
    assert code == 200 and decode(jpeg).size == (640, 360)
    page.locator('#diagnostics summary').click()
    page.locator('#pause').click()
    eventually(lambda: server.stats()['viewers'] == 0 and server.stats()['webrtc']['peers'] == 0)
    assert page.evaluate('video.srcObject === null && pc === null')
    page.locator('#pause').click()
    playing(page)
    page.locator('#transport').click()
    page.wait_for_function("playback === 'jpeg' && img.naturalWidth > 0")
    eventually(lambda: server.stats()['webrtc']['peers'] == 0 and server.stats()['viewers'] == 1)
    page.locator('#transport').click()
    playing(page)
    # Real connection loss must trigger fallback and release the RTC peer.
    page.evaluate('pc.close()')
    page.wait_for_function("playback === 'jpeg' && fallbackReason.length > 0", timeout=12000)
    eventually(lambda: server.stats()['webrtc']['peers'] == 0)
    assert page.locator('#transport').inner_text() == 'Video: Auto'
    page.locator('#transport').click()
    page.wait_for_function("playback === 'jpeg' && preferRtc === false")
    page.locator('#transport').click()
    playing(page)
    second = browser.new_page()
    second.goto(server.url, wait_until='domcontentloaded')
    playing(second)
    eventually(lambda: server.stats()['webrtc']['connected'] == 2)
    second.close()
    eventually(lambda: server.stats()['webrtc']['connected'] == 1)
    # Pause while the answer is in flight; a late response must release its peer.
    page.locator('#pause').click()
    page.evaluate("""() => {
        const original = window.fetch;
        window.fetch = async (...args) => {
            const response = await original(...args);
            if (args[0] === 'webrtc/offer') await new Promise(resolve => setTimeout(resolve, 700));
            return response;
        };
    }""")
    page.locator('#pause').click()
    eventually(lambda: server.stats()['webrtc']['peers'] > 0)
    page.locator('#pause').click()
    eventually(lambda: server.stats()['webrtc']['peers'] == 0)
    assert page.evaluate('paused && pc === null')
    page.locator('#pause').click()
    playing(page)
    # The existing source-loss contract must clear the video element, too.
    page.route('**/stats', lambda route: route.fulfill(json={**server.stats(), 'state': 'ended', 'error': 'Selected window closed'}))
    page.get_by_role('heading', name='OmaBeam disconnected').wait_for()
    assert page.evaluate('video.srcObject === null && pc === null')
    eventually(lambda: server.stats()['viewers'] == 0)
    page.close()
    assert not errors, errors
    print('PASS WebRTC: decoded H.264 pixels on LAN HTTP, diagnostics, lazy JPEG snapshots, pause/resume, transport switching, loss fallback, two peers, late answer cleanup, source loss')


def signaling(server, browser):
    page = browser.new_page()
    page.goto(server.url, wait_until='domcontentloaded')
    page.wait_for_function('latestStats !== null')
    page.locator('#pause').click()
    eventually(lambda: server.stats()['webrtc']['peers'] == 0)
    offer = page.evaluate("""async () => {
        const peer = new RTCPeerConnection(); peer.addTransceiver('video', {direction: 'recvonly'});
        const offer = await peer.createOffer(); peer.close(); return offer;
    }""")
    assert post(server, 'webrtc/offer', offer, {'Origin': 'http://unrelated.example'})[0] == 400
    assert post(server, '../wrong/webrtc/offer', offer)[0] == 404
    assert post(server, 'webrtc/offer', {'type': 'offer', 'sdp': 'invalid'})[0] == 400
    assert post(server, 'webrtc/offer', {**offer, 'sdp': offer['sdp'].replace('a=recvonly', 'a=recvonly\r\na=sendrecv')})[0] == 400
    assert server.stats()['webrtc']['peers'] == 0
    peers = []
    for _ in range(8):
        code, body = post(server, 'webrtc/offer', offer)
        assert code == 200, body
        peers.append(json.loads(body)['id'])
    assert post(server, 'webrtc/offer', offer)[0] == 400
    assert server.stats()['webrtc']['peers'] == 8
    for peer in peers:
        assert post(server, 'webrtc/close', {'id': peer})[0] == 200
    eventually(lambda: server.stats()['webrtc']['peers'] == 0)
    # Content-Length/body parsing must preserve bytes read with the headers and
    # reject oversized or ambiguous framing before allocating a peer.
    url = urllib.parse.urlsplit(server.url)
    for header in [b'Content-Length: 65537\r\n', b'Content-Length: 2\r\nContent-Length: 3\r\n', b'Transfer-Encoding: chunked\r\nContent-Length: 2\r\n']:
        with socket.create_connection((url.hostname, url.port), timeout=3) as stream:
            stream.sendall(f'POST {url.path}webrtc/offer HTTP/1.1\r\nContent-Type: application/json\r\n'.encode() + header + b'\r\n{}')
            assert b'400 Bad Request' in stream.recv(4096)
    # Unanswered offers expire instead of retaining a viewer indefinitely.
    assert post(server, 'webrtc/offer', offer)[0] == 200
    eventually(lambda: server.stats()['webrtc']['peers'] == 0, timeout=16)
    page.close()
    print('PASS WebRTC signaling: token/origin checks, bounded JSON, compatible offers, eight-peer cap, close, abandoned-offer expiry')


def fallback(server, browser):
    for init, offer in [
        ("window.RTCPeerConnection = undefined", None),
        ('', lambda route: route.abort()),
        # The page's display lease lapsed, for example during an outage.
        ('', lambda route: route.fulfill(status=409, body='This display is already connected to another device.')),
    ]:
        page = browser.new_page()
        if init: page.add_init_script(init)
        if offer: page.route('**/webrtc/offer', offer)
        page.goto(server.url, wait_until='domcontentloaded')
        page.wait_for_function("playback === 'jpeg' && img.naturalWidth > 0")
        assert page.locator('#transport-note').inner_text().startswith('JPEG fallback')
        if offer:
            # An offer that never reached the host, or one refused until the
            # lease is claimed again, may succeed on the next reconnect.
            assert page.evaluate('preferRtc') is True
            assert page.locator('#transport').inner_text() == 'Video: Auto'
        page.close()
    eventually(lambda: server.stats()['viewers'] == 0)
    print('PASS WebRTC fallback: missing browser API, failed signaling, and a lapsed display lease still display JPEG')


def sticky_fallback(server, browser):
    page = browser.new_page()
    errors, offers = [], []
    page.on('pageerror', lambda error: errors.append(str(error)))
    page.on('request', lambda request: offers.append(request.url) if 'webrtc/offer' in request.url else None)
    page.route('**/webrtc/offer', lambda route: route.fulfill(status=503, body='no'))
    page.goto(server.url, wait_until='domcontentloaded')
    page.wait_for_function("playback === 'jpeg' && img.naturalWidth > 0")
    assert page.evaluate('preferRtc === false')
    assert page.locator('#transport').inner_text() == 'Video: JPEG'
    assert page.locator('#transport-note').inner_text().startswith('JPEG fallback')
    # A JPEG reconnect must not retry H.264 that never played.
    count = len(offers)
    page.evaluate("img.dispatchEvent(new Event('error'))")
    page.wait_for_function("playback === 'jpeg' && img.naturalWidth > 0")
    page.wait_for_timeout(2000)
    assert len(offers) == count, offers
    assert page.locator('#transport-note').inner_text().startswith('JPEG fallback')
    page.unroute('**/webrtc/offer')
    page.locator('#transport').click()
    playing(page)
    page.close()
    assert not errors, errors
    print('PASS WebRTC sticky fallback: failed negotiation stays on JPEG across reconnects until Auto is selected')


def outage_fallback(server, browser):
    page = browser.new_page()
    errors = []
    page.on('pageerror', lambda error: errors.append(str(error)))
    page.goto(server.url, wait_until='domcontentloaded')
    playing(page)
    # While status polls fail the host is unreachable, so a failed negotiation
    # says nothing about H.264 and must not make JPEG sticky.
    page.route('**/stats', lambda route: route.abort())
    page.wait_for_function('failures > 0')
    page.route('**/webrtc/offer', lambda route: route.fulfill(status=503, body='no'))
    page.evaluate('connectRtc()')
    page.wait_for_function("playback === 'jpeg'")
    assert page.evaluate('preferRtc') is True
    assert page.locator('#transport').inner_text() == 'Video: Auto'
    page.unroute('**/webrtc/offer')
    page.unroute('**/stats')
    page.close()
    assert not errors, errors
    print('PASS WebRTC outage fallback: an H.264 failure while status polls fail stays retryable')


def static_capture(server, browser, output, sway_socket):
    page = browser.new_page()
    page.goto(server.url, wait_until='domcontentloaded')
    page.wait_for_function("playback === 'webrtc' && browserStats?.frames > 2", timeout=15000)
    assert page.evaluate('video.videoWidth') == 640
    page.locator('#pause').click()
    eventually(lambda: server.stats()['viewers'] == 0)
    # The visible screen is static. An unchanged-generation encoder test also
    # covers compositors that withhold captures until there is damage.
    page.locator('#pause').click()
    page.wait_for_function("playback === 'webrtc' && browserStats?.frames > 2", timeout=15000)
    second = browser.new_page()
    second.goto(server.url, wait_until='domcontentloaded')
    second.wait_for_function("playback === 'webrtc' && browserStats?.frames > 2", timeout=15000)
    second.close()
    if sway_socket:
        subprocess.run(['swaymsg', '-s', sway_socket, 'output', output, 'mode', '800x600'], check=True, capture_output=True)
        page.wait_for_function("video.videoWidth === 800 && video.videoHeight === 600", timeout=10000)
        # Exiting this private compositor removes the actual capture source.
        # Disabling an output leaves its Wayland object alive on some Sway versions.
        subprocess.run(['swaymsg', '-s', sway_socket, 'exit'], check=False, capture_output=True)
        page.get_by_role('heading', name='OmaBeam disconnected').wait_for(timeout=15000)
        assert page.evaluate('video.srcObject === null && pc === null')
        eventually(lambda: server.stats()['webrtc']['peers'] == 0)
    page.close()
    print('PASS WebRTC compositor: static screen, pause/resume, late viewer, live resolution change, actual source loss')


def dimension_checks(binary, browser):
    for width in (321, 1):
        with Server(binary, ['--bind', '0.0.0.0', '--webrtc', '--webrtc-port', '0', '--width', str(width)]) as server:
            page = browser.new_page()
            page.goto(server.url, wait_until='domcontentloaded')
            if width == 321:
                page.wait_for_function("playback === 'webrtc' && video.videoWidth === 322 && video.videoHeight === 180", timeout=15000)
                assert server.stats()['width'] == 321
                code, image = server.get('frame.jpg')
                assert code == 200 and decode(image).size == (321, 180)
            else:
                page.wait_for_function("playback === 'jpeg' && img.naturalWidth === 1", timeout=15000)
                assert '16 pixels' in server.stats()['webrtc']['error']
                eventually(lambda: server.stats()['webrtc']['peers'] == 0)
            page.close()
    print('PASS WebRTC sizes: one-pixel edge padding and JPEG fallback after an encoder size error')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--binary', default='target/debug/omabeam')
    parser.add_argument('--screenshot', type=Path)
    parser.add_argument('--browser-executable', default=os.environ.get('OMABEAM_TEST_CHROMIUM') or shutil.which('google-chrome') or shutil.which('chromium'))
    parser.add_argument('--capture-output')
    parser.add_argument('--sway-socket')
    parser.add_argument('--fps', type=int, default=60)
    parser.add_argument('--encoder', choices=['auto', 'hardware', 'software'], default='auto')
    args = parser.parse_args()
    source = ['--live', 'output', args.capture_output] if args.capture_output else None
    with Server(args.binary, ['--bind', '0.0.0.0', '--webrtc', '--webrtc-port', '0', '--fps', str(args.fps), '--native-pixels', '--encoder', args.encoder], source) as server:
        with sync_playwright() as p:
            browser = p.chromium.launch(headless=True, executable_path=args.browser_executable)
            try:
                if args.capture_output:
                    static_capture(server, browser, args.capture_output, args.sway_socket)
                else:
                    checks(server, browser, args.screenshot)
                    selected = server.stats()['webrtc']['encoder']
                    if args.encoder == 'hardware':
                        assert selected != 'OpenH264 software', selected
                    print(f'PASS selected encoder: {selected}')
                    signaling(server, browser)
                    fallback(server, browser)
                    sticky_fallback(server, browser)
                    outage_fallback(server, browser)
                    dimension_checks(args.binary, browser)
            finally:
                browser.close()


if __name__ == '__main__':
    main()
