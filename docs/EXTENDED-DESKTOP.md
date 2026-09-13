# Extended desktop implementation

The requested outcome is a real extra Hyprland desktop displayed in OmaBeam's
browser viewer. Windows can move onto it using the host's normal keyboard and
mouse. The existing physical displays remain in place.

Implementation and verification requirements:

- Add an Extend desktop choice to the standalone picker, with resolution,
  desktop scale, and left/right/above/below placement. Show the intended layout
  before creating the display. Keep this choice out of portal and screenshot modes.
- Add a CLI source (`--live extend WIDTH HEIGHT SCALE POSITION`) that follows
  the same lifecycle as the picker. Reuse the current JPEG/WebRTC viewer and
  transport settings, including fullscreen and fallback.
- Create a uniquely named Hyprland headless output through the native command
  socket. Configure and verify its dimensions, scale, and non-overlapping
  placement before capturing it. Do not persist changes to Hyprland config.
- The sharing process owns the output. Remove it on normal stop, termination,
  failed startup, or ended capture. Preserve recovery information if cleanup
  fails; recover an abandoned owned output after a crash. Never remove a
  pre-existing or unrelated display. Serialize session startup and recovery.
- Report the extended display in session status and provide clear instructions
  for moving windows and ending the session. A viewer disconnect keeps the
  desktop available for reconnection; stopping the host share removes it.
- Verify configuration, placement, IPC commands, rollback, termination, stale
  recovery, and concurrent-start behavior. Exercise actual virtual-output
  creation and browser capture on Linux, and inspect the native picker layout.
- Update installation/development usage and continuous-integration checks.

The receiver is the existing browser viewer. This implementation uses the
host's input devices; browser-to-host keyboard/touch injection, multiple
simultaneous independently configured displays, USB device discovery, and the
OpenDisplay wire protocol are separate features.

## Verification status — 2026-09-13

Passed the macOS and Linux Rust suites, six extended-display lifecycle tests,
five packaging tests, and thirteen firewall tests. Browser smoke tests passed,
as did actual output/region capture from a software-rendered Sway compositor.
The native picker was visually checked in landscape and portrait, including
switching back to screenshot mode.

On 2026-09-13, the user confirmed that they had tested extended desktop on
Omarchy and it was working, resolving the remaining live-verification blocker.
This is user-reported manual acceptance; the automated live acceptance script
was not run by the agent. The procedure in `docs/DEVELOPMENT.md` remains
available for repeatable checks of output creation, browser playback and
reconnection, and removal.
