# Native Cast implementation and qualification

Work is on `codex/native-google-cast`. Initial 720p synthetic video played on
both a Google Nest Hub and an E65-E1 TV, with user-confirmed visible animation.
The user also confirmed 1080p animation on the TV.
On the Omarchy laptop, the user confirmed labeled animations captured from real
Hyprland extended displays: 720p on the Nest Hub and 1080p on the TV.
This is an implementation in qualification, not a measured TV latency claim.
The design and acceptance gates remain in [NATIVE-CAST.md](NATIVE-CAST.md).
Detailed evidence is in [the feasibility report](qa/native-cast-feasibility.md).

## Implemented

- A separate C++ `omabeam-cast` process, built against the pinned Open Screen
  revision in `native/omabeam-cast/upstream.json`.
- mDNS receiver discovery, Cast device authentication, mirroring-app launch,
  video-only H.264 OFFER/ANSWER, encrypted RTP/RTCP, congestion feedback,
  keyframe recovery, control PING/PONG heartbeats, bounded IPC and graceful
  session-specific Stop.
- A Rust helper client with independent control/media channels, payload
  validation, bounded event/log storage and child-process cleanup.
- Shared conversion and hardware/software H.264 backends for browser and Cast.
- Native CLI capture for output/window/region and owned extended displays.
  Cast sessions create no HTTP/WebRTC listener. Extended displays are created
  after negotiation, and capture is released before removing the output.
- Cast status metadata and bar-panel presentation with receiver name,
  connecting/streaming/reconnecting state and Stop; browser link controls are hidden.
- Native picker destination and stable receiver selection, 720p/1080p and
  15/30 fps profiles, encoder choice, and restoration of browser settings.
- A 15-second recovery window that queries the same receiver session and never
  relaunches the receiver app. App replacement/explicit Stop is terminal.
- Optional source installation, Linux bundle validation, binary-bound C++
  dependency notices, and Linux software-receiver CI configuration.

## Reproducible local checks

```sh
cargo build --locked
python3 scripts/build-cast.py --sync --upstream
python3 tests/native_cast.py
python3 tests/native_cast_stream.py --app
cargo test --workspace --locked
python3 tests/omarchy_ui.py --screenshots target/cast-omarchy-qa
python3 tests/packaging.py
```

The upstream reference receiver needs FFmpeg, SDL2, Opus and VPX development
libraries. These are not runtime dependencies of the production Cast helper.
The first build downloads the pinned upstream dependency/compiler toolchain and
needs several gigabytes of disk space. The software stream tests bind loopback
sockets, disable discovery, use temporary developer certificates and generate
synthetic video. The GUI test needs the PySide6 environment described in
[DEVELOPMENT.md](DEVELOPMENT.md).

On macOS arm64, September 23, 2026:

| Check | Evidence |
| --- | --- |
| Native IPC | Malformed JSON, oversized lengths, version mismatch, EOF, truncated media and Stop during a partial media packet passed. |
| 720p30 offer | 150 accepted, 148 released in the last feedback snapshot, **150 actually decoded** by the software receiver. |
| 1080p30 offer | 150 accepted, 150 released in the last feedback snapshot, **150 actually decoded** by the software receiver. |
| Full Omabeam synthetic capture | 69 frames decoded and a control heartbeat reply; successful startup while the configured HTTP port was occupied; clean Stop and status removal. |
| Authentication | Production trust roots rejected the software receiver's developer certificate; an explicit developer certificate was needed for positive tests. |
| LAN discovery | Read-only discovery returned a Google Nest Hub and an E65-E1 TV with video capability. |
| Physical receiver smoke | User confirmed visible 720p animation on both devices. Release-app hardware-encoded synthetic sessions ran for 90 seconds, each returned 18 heartbeat replies, and stopped cleanly; see the feasibility report for counters and rate limitations. Nest Hub's reported recommendations/constraints excluded 1080p; the E65-E1 negotiated it. |
| Recovery safety | A wrong-session resume was rejected without interrupting decoding; killing the owned helper ended with `receiver_replaced` and did not launch again. |
| Workspace regression | Rust workspace tests passed (two existing tests ignored), including conversion, decoder recovery and capture/status lifecycle. |
| Browser regression | Real Chrome decoded H.264 through the shared Apple VideoToolbox backend; pause/resume, multiple peers, JPEG fallback, signaling and source-loss checks passed. |
| Encoder and display ownership | Hardware probe passed on Apple VideoToolbox; all six simulated Hyprland display lifecycle tests passed. These do not qualify Linux capture or its GPU drivers. |
| Bar panel | Offscreen status/controller/keyboard/pointer tests passed, including Cast connecting, streaming and reconnecting states. |
| Packaging | Seven installer/archive tests passed, including native helper architecture, binary/notice hashes and unsafe notice paths. |

Software decoder traces prove decoding through the software receiver. They do not measure
capture-to-TV latency, prove 30 fps sustained performance, or establish physical
receiver compatibility. `released` includes transport ACK/cancellation and is
not a presentation counter.

On Omarchy 4.0.4 x86_64 with Hyprland 0.56.2, the release app and native helper
built and passed 121 Rust tests (two ignored), native IPC, seven package checks,
and the software decoder fixtures at 720p/1080p. Real Hyprland capture passed
for window, output, region, and an owned extended display, using isolated
generated content. The user confirmed 720p extended-display animation on the
Nest Hub and 1080p on the TV; normal Stop removed each status and temporary
output. The 1080p run admitted about 13 encoded frames per second, while the
720p Nest Hub run admitted about 30; these are not displayed-frame measurements.
Auto encoding fell back to OpenH264 with the laptop's installed drivers, so these results do
not qualify VA-API or NVENC. Counters and reproducible physical-test commands
are in [the feasibility report](qa/native-cast-feasibility.md).

## Development commands

```sh
omabeam --cast-devices
omabeam --cast-demo RECEIVER_ID
omabeam --cast RECEIVER_ID -- output DP-1
omabeam --cast RECEIVER_ID -- region DP-1 0 0 1280 720
omabeam --width 1920 --cast RECEIVER_ID -- extend 1920 1080 1 right
omabeam --stop
```

The receiver ID comes from discovery. Startup resolves that same ID again;
it never substitutes a different device with the same display name. The default
canvas is 1280×720, `--width 1920` selects 1920×1080, and sources are fitted with
black bars while preserving aspect ratio. Cast currently caps frame rate at 30.
`--cast-demo RECEIVER_ID` uses generated frames and the standard Google trust
roots. `--cast-test IP:PORT CERTIFICATE` is the explicit software-receiver
fixture command; ordinary Cast sessions never use developer certificates.

## Still required

Broader real-device H.264 testing, extended-display recovery under faults, Linux GPU paths, Linux CI/bundle execution,
glass-to-glass latency samples, successful network-resume behavior on physical
receivers and long soak tests remain acceptance gates. The native GPUI picker
has been compiled; its receiver flow still needs interaction on the Linux host.
No release or support claim should precede these checks. System audio is a
separate follow-up milestone after the video beta; this implementation is video-only.

## Network checks

1. Run `omabeam --cast-devices`. An empty list means discovery did not return
   a video receiver; it does not prove a firewall block. Check that the receiver
   is awake and on the same LAN, that guest/client isolation is disabled for
   these devices, and that the interface can receive mDNS multicast on UDP 5353.
2. Use the returned ID with `omabeam --cast ID -- output NAME`. The session log
   at `$XDG_RUNTIME_DIR/omabeam/live.log` distinguishes discovery, connection,
   authentication, launch and negotiation failures. Connection failure can mean
   the advertised address/port is unreachable; certificate failure is not fixed
   by opening browser ports.
3. If negotiation succeeds but feedback stops, inspect UDP media/RTCP delivery
   between the two LAN devices. The Cast UDP ports are negotiated; browser
   port 9848 is not the Cast transport. Check local firewall state with your
   firewall's read-only status command (for UFW, `sudo ufw status verbose`).
   Discovery across VLANs and IPv6 behavior require separate network validation.

OmaBeam does not change Cast firewall rules automatically. These checks provide
diagnostics, not a claim that an empty discovery result identifies the cause.
