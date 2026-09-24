#!/usr/bin/env python3
"""Decode native Cast video over loopback. Requires the --upstream native build.

Uses temporary developer credentials and synthetic pixels; never discovers or
launches a physical receiver. The receiver's decoder traces are the evidence,
not the sender's enqueue or RTP counters.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import re
import signal
import socket
import subprocess
import tempfile
import time
import native_cast as ipc

ROOT = Path(__file__).resolve().parents[1]
NATIVE = ROOT / "target/native-cast/openscreen/out/omabeam"


def stop(proc):
    if proc.poll() is None:
        proc.terminate()
        try:
            proc.wait(timeout=4)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
            raise AssertionError("Process did not stop within four seconds")


def decoded(path):
    return path.read_text(errors="replace").count("Frame.Decode.End")


def resume_guard(endpoint, certificate):
    with ipc.helper(certificate) as (proc, _):
        proc.stdin.write(ipc.packet({"version":1, "command":"connect", "endpoint":endpoint,
            "width":1280, "height":720, "fps":30, "bitrate":4000000,
            "resume_session":"different-session-must-not-be-adopted"}))
        until = time.monotonic() + 5
        while time.monotonic() < until:
            value = ipc.event(proc)
            assert value["event"] != "negotiated", "Resume adopted an unrelated receiver session"
            if value["event"] == "error":
                assert value["code"] == "receiver_replaced", value
                assert proc.wait(timeout=2) == 1
                return
        raise AssertionError("Wrong-session resume did not fail")


def authentication_guard(endpoint):
    # A test receiver must fail with ordinary production trust roots. Only the
    # explicit development certificate may authorize this fixture.
    with ipc.helper() as (proc, _):
        proc.stdin.write(ipc.packet({"version":1, "command":"connect", "endpoint":endpoint,
            "width":1280, "height":720, "fps":30, "bitrate":4000000}))
        until = time.monotonic() + 5
        while time.monotonic() < until:
            value = ipc.event(proc)
            assert value["event"] != "negotiated", "Production trust accepted a development certificate"
            if value["event"] == "error":
                assert value["code"] == "authentication", value
                assert proc.wait(timeout=2) == 1
                return
        raise AssertionError("Untrusted receiver was not rejected")


def run_app(directory, endpoint, certificate, receiver_log, binary, fault=False):
    runtime = directory / "runtime"
    runtime.mkdir(mode=0o700)
    # Keep an HTTP port occupied: Cast must work without trying to bind it.
    with socket.socket() as occupied, (directory / "app.log").open("w+") as log:
        occupied.bind(("127.0.0.1", 0))
        occupied.listen()
        env = {**os.environ, "XDG_RUNTIME_DIR": str(runtime)}
        app = subprocess.Popen([str(binary), "--encoder", "software",
            "--bind", "127.0.0.1", "--port", str(occupied.getsockname()[1]),
            "--cast-test", endpoint, str(certificate)], stdout=log, stderr=log, env=env)
        before = decoded(receiver_log)
        observed = None
        try:
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                assert app.poll() is None, f"App exited: {(directory / 'app.log').read_text()}"
                status = runtime / "omabeam/live.json"
                if status.exists():
                    value = json.loads(status.read_text())
                    assert not value.get("error"), value
                    assert value["url"] == "", "Cast exposed a browser URL"
                    if (value.get("cast", {}).get("accepted_frames", 0) >= 30
                            and value["cast"].get("control_heartbeats", 0) >= 1):
                        observed = value
                        break
                time.sleep(0.05)
            assert observed, "App did not begin streaming"
            assert decoded(receiver_log) - before >= 10, "App frames were not decoded"
            if fault:
                # Kill only the helper owned by this test's app. The software
                # receiver ends its session on disconnect, so recovery must
                # report that loss instead of automatically launching again.
                processes = subprocess.check_output(["ps", "-eo", "pid,ppid,comm"], text=True)
                children = [int(parts[0]) for line in processes.splitlines()[1:]
                            if len(parts := line.strip().split(None, 2)) == 3
                            and parts[1] == str(app.pid) and Path(parts[2]).name == "omabeam-cast"]
                assert len(children) == 1, children
                os.kill(children[0], signal.SIGKILL)
                assert app.wait(timeout=20) != 0, "Lost receiver session was silently relaunched"
                ended = json.loads(status.read_text())
                assert ended["state"] == "ended" and ended["cast"]["connection"] == "failed", ended
                assert "receiver_replaced" in ended["error"], ended
                return {"helper_crash": True, "no_relaunch": True, "ended_status": True}
            before_guard = decoded(receiver_log)
            resume_guard(endpoint, certificate)
            deadline = time.monotonic() + 5
            while decoded(receiver_log) < before_guard + 10 and time.monotonic() < deadline:
                assert app.poll() is None, "Rejected resume stopped the original sender"
                time.sleep(0.05)
            assert decoded(receiver_log) >= before_guard + 10, "Rejected resume disrupted playback"
        finally:
            stop(app)
        assert app.returncode == 0, (directory / "app.log").read_text()
        assert not (runtime / "omabeam/live.json").exists(), "Cast left an active status"
        return {"accepted": observed["cast"]["accepted_frames"],
                "decoded": decoded(receiver_log) - before,
                "no_http_listener": True, "clean_stop": True, "resume_ownership_guard": True,
                "control_heartbeats": observed["cast"]["control_heartbeats"]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--app", action="store_true", help="Also test the full Omabeam demo pipeline")
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/omabeam",
                        help="App binary for --app (allows qualification of release builds)")
    options = parser.parse_args()
    # The IPC test module accepts a positional executable in standalone mode.
    ipc.HELPER = NATIVE / "omabeam-cast"
    # Generating and converting 150 full-HD images in an unoptimized example
    # can exceed the transport deadline on otherwise capable Linux hosts.
    subprocess.run(["cargo", "build", "--release", "--locked", "-p", "omabeam-cast", "--example", "demo"],
                   cwd=ROOT, check=True)
    report = {"receiver": "Open Screen software receiver on loopback", "profiles": {}}
    with tempfile.TemporaryDirectory(prefix="omabeam-cast-") as temporary:
        directory = Path(temporary)
        subprocess.run([str(NATIVE / "cast_receiver"), "-g"], cwd=directory, check=True,
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        certificate = directory / "generated_root_cast_receiver.crt"
        key = directory / "generated_root_cast_receiver.key"
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        endpoint = f"127.0.0.1:{port}"
        log_path = directory / "receiver.log"
        env = {**os.environ, "SDL_VIDEODRIVER": "dummy", "SDL_AUDIODRIVER": "dummy"}
        with log_path.open("w+") as log:
            receiver = subprocess.Popen([str(NATIVE / "cast_receiver"), "-x", "-q", "-t",
                "-d", str(certificate), "-p", str(key), "-r", str(port),
                "lo0" if platform.system() == "Darwin" else "lo"], env=env, stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    assert receiver.poll() is None, log_path.read_text()
                    if "CastService is running" in log_path.read_text():
                        time.sleep(0.1)
                        break
                    time.sleep(0.05)
                authentication_guard(endpoint)
                report["production_trust_rejects_fixture"] = True
                for profile in ("720p", "1080p"):
                    before = decoded(log_path)
                    result = subprocess.run([str(ROOT / "target/release/examples/demo"),
                        str(NATIVE / "omabeam-cast"), endpoint, str(certificate), profile],
                        text=True, capture_output=True, timeout=45)
                    assert result.returncode == 0, result.stdout + result.stderr + log_path.read_text()[-4000:]
                    count = decoded(log_path) - before
                    match = re.search(r"accepted=(\d+) released=(\d+)", result.stdout)
                    assert match and count >= 145, (count, result.stdout)
                    report["profiles"][profile] = {"accepted":int(match[1]),
                        "released":int(match[2]), "decoded":count}
                if options.app:
                    report["app"] = run_app(directory, endpoint, certificate, log_path, options.binary.resolve())
                    fault_directory = directory / "fault"
                    fault_directory.mkdir()
                    report["recovery"] = run_app(fault_directory, endpoint, certificate, log_path,
                                                 options.binary.resolve(), fault=True)
                assert "FATAL:" not in log_path.read_text()
            finally:
                stop(receiver)
    output = ROOT / "target/native-cast/qualification.json"
    output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
