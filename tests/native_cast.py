#!/usr/bin/env python3
"""Native helper IPC/lifecycle tests. No receiver or desktop is contacted.

Build with scripts/build-cast.py first. An optional helper path may be passed.
"""
import contextlib
import json
from pathlib import Path
import select
import socket
import struct
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]
HELPER = Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "target/debug/omabeam-cast"


def packet(value):
    data = json.dumps(value).encode()
    return struct.pack("!I", len(data)) + data


def exact(pipe, length):
    out = b""
    until = time.monotonic() + 3
    while len(out) < length:
        assert select.select([pipe], [], [], max(0, until - time.monotonic()))[0], "event timeout"
        chunk = pipe.read(length - len(out))
        assert chunk, "unexpected helper EOF"
        out += chunk
    return out


def event(proc):
    length, = struct.unpack("!I", exact(proc.stdout, 4))
    assert 0 < length <= 4096
    value = json.loads(exact(proc.stdout, length))
    assert value["version"] == 1
    return value


@contextlib.contextmanager
def helper(certificate=None):
    parent, child = socket.socketpair()
    args = [str(HELPER), "--media-fd", str(child.fileno())]
    if certificate:
        args += ["--developer-certificate", str(certificate)]
    proc = subprocess.Popen(args,
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, pass_fds=(child.fileno(),), bufsize=0)
    child.close()
    try:
        assert event(proc)["event"] == "ready"
        yield proc, parent
    finally:
        parent.close()
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
        proc.stdin.close()
        proc.stdout.close()
        proc.stderr.close()


def reject(data, media=False, close=False):
    with helper() as (proc, stream):
        if media:
            stream.sendall(data)
            if close:
                stream.shutdown(socket.SHUT_WR)
        else:
            proc.stdin.write(data)
            if close:
                proc.stdin.close()
        assert event(proc)["event"] == "error"
        assert proc.wait(timeout=2) == 1


def main():
    assert HELPER.is_file(), f"Build the helper first: {HELPER}"
    version = subprocess.check_output([str(HELPER), "--version"], text=True)
    assert "protocol=1" in version
    with helper() as (proc, media):
        # Incomplete media must never hold Stop behind a payload read.
        media.sendall(packet({"bytes": 2 * 1024 * 1024}) + b"partial")
        start = time.monotonic()
        proc.stdin.write(packet({"version": 1, "command": "stop"}))
        assert proc.wait(timeout=1) == 0
        assert time.monotonic() - start < 1
    with helper() as (proc, _):
        proc.stdin.close()
        assert proc.wait(timeout=1) == 0
    for data in [struct.pack("!I", 0), struct.pack("!I", 4097), struct.pack("!I", 0xffffffff),
                 struct.pack("!I", 1) + b"{", packet([]),
                 packet({"version": 2, "command": "stop"}),
                 packet({"version": 1, "command": "stop", "bytes": 1}),
                 packet({"version": 1, "command": "stop", "bytes": -1}),
                 packet({"version": 1, "command": "unknown"}),
                 packet({"version": 1, "command": "connect", "endpoint": "localhost:8009"})]:
        reject(data)
    for identity in ("", 1, "x" * 257):
        reject(packet({"version": 1, "command": "connect", "endpoint": "127.0.0.1:8009",
                       "width": 1280, "height": 720, "fps": 30, "bitrate": 4000000,
                       "resume_session": identity}))
    reject(packet({"bytes": 2 * 1024 * 1024 + 1}), media=True)
    reject(b"\0\0", close=True)
    reject(packet({"bytes": 100}) + b"partial", media=True, close=True)
    print("Native Cast IPC: version, bounds, malformed input, EOF, and independent Stop passed")


if __name__ == "__main__":
    main()
