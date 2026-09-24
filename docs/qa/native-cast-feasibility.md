# Native Cast feasibility evidence

Date: 2026-09-23. Branch: `codex/native-google-cast`. Sender hosts: macOS 27.0
arm64 and Omarchy 4.0.4 x86_64. Initial Mac Rust app/examples were debug builds;
subsequent 90-second Mac tests and Linux capture tests used the release app.
The native Open Screen helper was optimized throughout.
These checks establish initial interoperability, not release
qualification or a capture-to-display latency claim.

## Revisions and reproduction

- Open Screen: `8b108491d2696309ca37ac0d3260bf423e773e8b`.
- depot_tools: `099d54a878f6554bb096e5e77d49b93973b92b24`.
- Upstream dependency/compiler revisions: the locked Open Screen `DEPS`.
- Rust dependencies: this branch's `Cargo.lock`.
- Build patches and notice inventory: `native/omabeam-cast/UPSTREAM.md`.

```sh
cargo build --locked
python3 scripts/build-cast.py --sync --upstream
python3 tests/native_cast.py
python3 tests/native_cast_stream.py --app
cargo test --workspace --locked
cargo build --locked -p omabeam-cast --example demo
```

After explicitly selecting a receiver from `omabeam --cast-devices`, the
synthetic encoder example can use normal Google device authentication:

```sh
target/debug/examples/demo target/debug/omabeam-cast RECEIVER_IP:PORT --production 720p
target/debug/examples/demo target/debug/omabeam-cast RECEIVER_IP:PORT --production 1080p
```

The full app's synthetic capture pipeline also supports a discovered receiver
ID, including normal Auto hardware selection, status and graceful Stop:

```sh
target/debug/omabeam --encoder auto --cast-demo RECEIVER_ID
```

Stop with Ctrl+C or `omabeam --stop` on Linux. These commands visibly replace
the selected receiver's current app. Development certificates are only used
for the loopback software receiver; they are never needed for these devices.

## Stock receiver observations

Both devices were discovered through the production mDNS path and tested with
the user's explicit selection. Their IDs, addresses and receiver session IDs
are omitted from this report. Their local device-info endpoints reported:

- Google Nest Hub: Cast build `3.80.546557`, system build `546557`, stable channel.
- E65-E1: Cast build `1.50.243780`, system build `243780`, stable channel.

These are the receiver-reported Cast/system build fields, not an independently
verified TV vendor firmware version.

| Receiver | Offered mode | Initial result |
| --- | --- | --- |
| Google Nest Hub | 1280×720, 30 fps, H.264 | Authentication, stock-app launch and negotiation succeeded. 150 access units were accepted and released. The user confirmed visible animation. |
| E65-E1 Cast-enabled TV | 1280×720, 30 fps, H.264 | Authentication, stock-app launch and negotiation succeeded. 143/150 access units were accepted; 143 were released. The user confirmed visible animation. |
| Google Nest Hub | 1920×1080, 30 fps, H.264 | OmaBeam ended negotiation with `unsupported_resolution` because the requested mode exceeded the receiver's recommendations/constraints. Use 720p. No virtual output was created. |
| E65-E1 Cast-enabled TV | 1920×1080, 30 fps, H.264 | Negotiated. Initial example: 147/150 access units accepted, 145 released in the last feedback sample. The user confirmed visible animation during the subsequent full-app release test. |

The example stops its own app session after each test. `accepted` means the
sender admitted an encoded frame, and `released` counts ACK/cancellation;
neither independently establishes decoded or displayed frames. The 720p display
evidence on both receivers and the TV's 1080p evidence are the user's
confirmations. An offered 30 fps is not a
measurement of sustained playback at that rate.

Additional full-app tests used synthetic capture and the real Apple VideoToolbox
encoder. All stopped cleanly and removed their status:

| Receiver / canvas / build / duration | Last transport counters | Last RTT sample | Control heartbeat replies |
| --- | --- | --- | --- |
| Nest Hub / 720p / debug / 20 s | 212 accepted, 212 released, 0 helper drops | 10.4 ms | Not measured |
| E65-E1 / 1080p / debug / 20 s | 98 accepted, 97 released, 0 helper drops | 38.5 ms | Not measured |
| Nest Hub / 720p / release / 90 s | 2,333 accepted, 2,333 released, 103 helper drops | 32.9 ms | 18 |
| E65-E1 / 1080p / release / 90 s | 2,245 accepted, 2,244 released, 312 helper drops | 44.6 ms | 18 |

These runs did not establish 30 fps encoding: the application retains
the latest captured frame while conversion/encoding catches up. Captured-frame
FPS was about 27, which must not be reported as encoded or displayed FPS. RTT
is a transport sample, not glass-to-glass latency. Linux measurements follow
below; sustained playback performance still needs qualification.

## Software and regression evidence

The software receiver's decoder traces recorded 150 decoded frames at both
720p and 1080p. The full synthetic app test recorded 69 decoded frames and a
control heartbeat reply while
its configured browser HTTP port was occupied, then stopped and removed its
active status. Production trust rejected the fixture's untrusted certificate.

A second helper's wrong-session resume was rejected while the first sender
continued decoding. Killing the owned helper ended with `receiver_replaced`
when the software receiver discarded the original session. No new mirroring
app was launched during recovery. Successful physical receiver reconnection
remains to be tested.

Workspace tests passed, as did seven package tests, six simulated Hyprland
lifecycle tests, and offscreen QML interaction/rendering tests. Targeted
encoder tests cover fresh IDRs after failure, aspect fitting, alpha and
preserving software fallback across bitrate changes. Real Chrome decoded H.264
through Apple VideoToolbox in the browser regression suite; its pause/resume,
multiple-peer, signaling, JPEG fallback and source-loss checks passed.

## Linux build and capture evidence

An isolated source snapshot was built on an x86_64 Omarchy 4.0.4 laptop,
Linux `7.2.5-3-omarchy`, Hyprland `0.56.2`, Rust `1.98.1`.
The release app, encoder helper, production Cast helper, and reference
sender/receiver built successfully. The Linux Rust suite passed 121 tests
(two existing tests ignored); seven packaging tests and native IPC tests passed.
Production discovery returned the same two selected receivers.

The reference build exposed two host-library integration issues, now handled
by `scripts/build-cast.py`: narrow library include trees preserve libc++ header
order, and reference executables use startup objects matching the host glibc.
The production helper retains the prescribed compiler/sysroot and links only
the standard C runtime libraries, not FFmpeg or SDL.

The actual GPU probe failed with this laptop's installed drivers. Auto mode
selected OpenH264 software, and the explicit-software/missing-helper/required-
hardware selection checks passed. This does not qualify VA-API or NVENC.

```sh
cargo build --release --locked
python3 scripts/build-cast.py --sync --upstream
cargo test --workspace --release --locked
python3 tests/native_cast.py target/release/omabeam-cast
python3 tests/native_cast_stream.py --app --binary target/release/omabeam
python3 tests/hardware_encoding.py --binary target/release/omabeam
```

The stream test builds an optimized synthetic sender. Its former debug sender
exceeded the 45-second full-HD generation timeout on this laptop, so that run
did not count as a transport failure or successful qualification.

The optimized Linux software-receiver fixture passed: 150 decoded frames at
720p, 149 at 1080p, and 167 from the full app. The full app admitted 148 frames,
received a control heartbeat, started while its browser HTTP port was occupied,
and stopped cleanly. The fixture also passed production certificate rejection,
wrong-session resume rejection, and helper-crash cleanup without relaunching
the receiver app. Decoder counts can include repeated presentations and are
not a physical display rate.

Real compositor tests used a generated QML window with colored bars, a moving
square, and a mode label. Window/output/region tests used a temporary headless
output; extended mode used OmaBeam's own negotiated virtual output. No existing
user window or physical desktop was selected. Auto encoding chose OpenH264.

| Receiver / source / canvas | Last transport counters | Heartbeat replies | Display evidence |
| --- | --- | --- | --- |
| Nest Hub / extended / 720p / 60 s | 1,775 accepted, 1,774 released, 0 helper drops | 12 | User confirmed the labeled animation. |
| E65-E1 / window / 720p / 14 s | 364 accepted, 363 released, 7 helper drops | 2 | Capture and transport verified; no separate visual confirmation. |
| E65-E1 / output / 720p / 14 s | 346 accepted, 346 released, 22 helper drops | 2 | Capture and transport verified; no separate visual confirmation. |
| E65-E1 / region / 720p / 14 s | 354 accepted, 353 released, 17 helper drops | 2 | Capture and transport verified; no separate visual confirmation. |
| E65-E1 / extended / 1080p / 90 s | 1,215 accepted, 1,214 released, 43 helper drops | 18 | User confirmed the labeled animation. |

All five sessions stopped cleanly and removed their status. Temporary outputs
were removed and the laptop's original monitor remained in place. The 14-second
runs used an earlier harness whose duration included discovery; the committed
test now measures its requested duration from the streaming state.

The 1080p run captured about 30 frames per second but admitted only about 13
encoded frames per second. The Nest Hub's 720p run admitted about 30 per second.
These are sender measurements: visible animation confirms playback, but neither
counter measures displayed frame rate or glass-to-glass latency. The 1080p
software path needs performance work before a sustained 30 fps claim.

One TV 1080p attempt ended before launch because the selected ID was absent
from its five-second discovery scan. The reported blank screen for that attempt
was not decoding evidence: no output was created and no video was sent. A
subsequent discovery scan found the same TV again, and the retry produced the
user-confirmed 1080p result above.

Reproduce the opt-in physical test only after selecting a receiver that may be
interrupted. Visual confirmation remains separate from its counters:

```sh
python3 tests/native_cast_hyprland.py RECEIVER_ID extended --seconds 60
python3 tests/native_cast_hyprland.py RECEIVER_ID window --seconds 20
python3 tests/native_cast_hyprland.py RECEIVER_ID output --seconds 20
python3 tests/native_cast_hyprland.py RECEIVER_ID region --seconds 20
python3 tests/native_cast_hyprland.py RECEIVER_ID extended --width 1920 --seconds 90
```

## Codec decision and remaining gates

Keep constrained-baseline H.264 as the initial video codec. Visible 720p
playback on a Google receiver and an independently implemented TV supports
continuing this implementation. There is no evidence here that requires VP8.

Still required: Linux GPU paths, physical reconnect/receiver replacement,
Linux CI and packaged-install execution, native picker interaction,
WAN-blocked behavior, sustained rates, long soak tests,
and measured glass-to-glass latency. Audio remains a separate post-video-beta
milestone. The original milestone 0 Linux/performance gates are not complete.
