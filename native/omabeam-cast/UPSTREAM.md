# Open Screen integration

`upstream.json` locks Open Screen and the dependency bootstrap tools. Open Screen's
DEPS locks its transitive sources and compiler/tools. Build with
`python3 scripts/build-cast.py --sync --upstream` from the repository root.
Subsequent local builds can omit `--sync`. Build output lives under
`target/native-cast/`; the main Cargo build does not download C++ dependencies.
Build the Rust app first; the script copies the helper beside existing
`target/debug` and `target/release` app builds. Source installation uses the
explicit helper path under its external cache.

`cast_agent.h` and `cast_agent.cc` adapt the authenticated socket, virtual
connection, application launch and shutdown flow from
`cast/standalone_sender/looping_file_cast_agent.{h,cc}` at the locked revision.
The file playback/remoting implementation is replaced by bounded encoded-frame
IPC. `discovery.{h,cc}` uses the same public discovery API as `receiver_chooser`.
The production GN target links library targets, not the test-only reference app.

The adapted source retains the Chromium copyright and accompanying BSD-style
license in `LICENSE.openscreen`. Distribution must also include the notices for
the complete linked dependency graph; the Cargo license bundle is insufficient.
The optional upstream sender/receiver are local development tools.

The helper uses Open Screen's C++ mDNS parser (`enable_rust=false`) and skips
the unrelated Chromium Rust compiler download. OmaBeam itself still uses Rust.

The build script makes only build-integration changes in the cached checkout:
it adds this executable to the discovery/platform/certificate visibility lists,
registers the overlay target, marks two parser-version fields
`[[maybe_unused]]` when the optional Rust parsers are disabled, and sets
`SessionConfig::max_in_flight_media_duration` to 150 ms in
`SenderSession::CreateSender`. That override is re-applied after every sync.
The default window is `clamp(2 * RTT, 66 ms, playout / 3)`, so on ethernet it
never grows past 66 ms and a late receiver checkpoint stalls the sender. The upstream
certificate and transport implementations are retained. Production linking does
not enable or link FFmpeg, SDL, Opus, or VPX; `--upstream` enables those libraries
only for the reference development executables.

On Linux, the reference tools receive a narrow header directory containing
links to FFmpeg/Opus/VPX/SDL headers. Adding the host's entire `/usr/include`
would incorrectly put glibc headers ahead of the pinned libc++ wrappers and
sysroot. The reference executables also use the host's C runtime startup objects
when linking those system libraries; modern glibc no longer supplies symbols
expected by the bundled Debian startup objects. The production helper keeps
its prescribed toolchain and sysroot.

IPC uses separate control and media streams. stdin carries control JSON,
`--media-fd` carries framed H.264 access units, stdout carries JSON events, and
stderr carries diagnostics. Headers are big-endian u32 length plus UTF-8 JSON,
limited to 4096 bytes. Media payloads are limited to 2 MiB; output events have a
256 KiB queue limit. Protocol version is 1. The Rust client is in
`crates/omabeam-cast`; `tests/native_cast.py` exercises malformed input and Stop
during a partial media packet.

`--developer-certificate` replaces the trust roots only for explicit local
software-receiver tests. Normal discovery and Cast sessions do not pass it.
