#!/usr/bin/env python3
"""Build the pinned native Cast dependencies in an isolated, ignored cache.

The upstream sender/receiver targets are development tools. The production
omabeam-cast target is an overlay maintained in native/omabeam-cast.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import shlex
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
NATIVE = ROOT / "native/omabeam-cast"


def run(args, cwd=None, env=None):
    print("+ " + shlex.join(map(str, args)), flush=True)
    subprocess.run(list(map(str, args)), cwd=cwd, env=env, check=True)


def checkout(path, spec, env):
    if not path.exists():
        run(["git", "init", "-q", path], env=env)
        run(["git", "-C", path, "remote", "add", "origin", spec["url"]], env=env)
    origin = subprocess.check_output(
        ["git", "-C", str(path), "remote", "get-url", "origin"], text=True
    ).strip()
    if origin != spec["url"]:
        raise RuntimeError(f"Unexpected Git origin in {path}: {origin}")
    head = subprocess.run(["git", "-C", str(path), "rev-parse", "HEAD"],
                          capture_output=True, text=True)
    if head.returncode == 0 and head.stdout.strip() == spec["revision"]:
        return
    dirty = subprocess.check_output(
        ["git", "-C", str(path), "status", "--porcelain", "--untracked-files=no"], text=True)
    if dirty:
        raise RuntimeError(f"Refusing to change revision with modified files in {path}")
    run(["git", "-C", path, "fetch", "--depth=1", "origin", spec["revision"]], env=env)
    run(["git", "-C", path, "checkout", "--detach", spec["revision"]], env=env)


def library_dirs(package, variable):
    value = subprocess.check_output(
        ["pkg-config", f"--variable={variable}", package], text=True).strip()
    if not value:
        raise RuntimeError(f"pkg-config did not find {variable} for {package}")
    # Resolve Homebrew symlinks to avoid including its top-level OpenSSL headers
    # when the pinned Open Screen build expects BoringSSL.
    path = Path(value).resolve()
    if platform.system() == "Darwin" and str(path).startswith("/opt/homebrew/"):
        # SDL's .pc file sometimes names the shared prefix, whose directory
        # itself is not a symlink. Follow an installed file, not a formula
        # alias (Homebrew can rename formulas without moving installed files).
        names = {"sdl2": ("SDL2/SDL.h", "libSDL2.dylib"),
                 "libavcodec": ("libavcodec/avcodec.h", "libavcodec.dylib"),
                 "opus": ("opus/opus.h", "libopus.dylib"),
                 "vpx": ("vpx/vpx_encoder.h", "libvpx.dylib")}
        name = names[package][0 if variable == "includedir" else 1]
        file = path / name
        if not file.is_file():
            raise RuntimeError(f"Missing dependency file: {file}")
        path = file.resolve().parents[len(Path(name).parts) - 1]
    return str(path)


def library_includes(package, include_root):
    directory = Path(library_dirs(package, "includedir"))
    if platform.system() != "Linux":
        return [str(directory)]
    # Host package directories can also contain glibc headers. Passing those
    # through GN include_dirs puts -I/usr/include ahead of the pinned libc++
    # wrappers and sysroot. Expose only the reference tools' library headers.
    names = {"libavcodec": ["libavcodec", "libavformat", "libavutil", "libswresample"],
             "opus": ["opus"], "vpx": ["vpx"], "sdl2": ["SDL2"]}[package]
    destination = include_root / package
    destination.mkdir(parents=True, exist_ok=True)
    for name in names:
        source = directory / name
        if not source.is_dir():
            raise RuntimeError(f"Missing dependency headers: {source}")
        link = destination / name
        if link.is_symlink():
            if link.resolve() == source.resolve():
                continue
            link.unlink()
        if link.exists():
            raise RuntimeError(f"Unexpected dependency header cache entry: {link}")
        link.symlink_to(source, target_is_directory=True)
    return [str(destination)]


def gn_arguments(upstream, include_root, target_cpu=None):
    args = {
        "is_debug": False,
        "symbol_level": 0,
        "enable_rust": False,
        "use_rust_mdns_parser": False,
        "build_python_bindings": False,
        "is_component_build": False,
    }
    if target_cpu:
        args["target_cpu"] = target_cpu
    if platform.system() == "Darwin":
        args["mac_deployment_target"] = platform.mac_ver()[0]
    if upstream:
        for prefix, package in [("ffmpeg", "libavcodec"), ("libopus", "opus"),
                                ("libvpx", "vpx"), ("libsdl2", "sdl2")]:
            args[f"have_{prefix}"] = True
            args[f"{prefix}_include_dirs"] = library_includes(package, include_root)
            args[f"{prefix}_lib_dirs"] = [library_dirs(package, "libdir")]
    return "\n".join(f"{key} = {json.dumps(value)}" for key, value in args.items()) + "\n"


def reference_startup_objects(source):
    # The reference executables link host FFmpeg/SDL libraries. Newer glibc
    # removed __libc_csu_init/fini, which the bundled Debian startup object
    # expects. Use the host's matching startup objects only for these tools;
    # retain the pinned compiler, C++ library and production-helper sysroot.
    startup = Path(subprocess.check_output(
        ["cc", "-print-file-name=Scrt1.o"], text=True).strip()).resolve()
    if not startup.is_file():
        raise RuntimeError("Could not locate host C runtime startup objects")
    marker = "# OmaBeam host startup objects"
    for target in ("cast_sender", "cast_receiver"):
        build = source / "cast" / target.replace("cast_", "standalone_") / "BUILD.gn"
        text = "".join(line for line in build.read_text().splitlines(keepends=True)
                       if marker not in line).rstrip() + "\n"
        anchor = f'openscreen_executable("{target}") {{'
        if anchor not in text:
            raise RuntimeError(f"Upstream reference target changed: {target}")
        flag = json.dumps("-B" + str(startup.parent) + "/")
        end = text.index("\n", text.index("configs = [", text.index(anchor)))
        text = text[:end] + f'\n  configs += [ ":omabeam_host_startup" ]  {marker}' + text[end:]
        text += f'\nconfig("omabeam_host_startup") {{ ldflags = [ {flag} ] }}  {marker}\n'
        build.write_text(text)


def collect_notices(source, output, gn, spec):
    graph = json.loads(subprocess.check_output(
        [str(gn), "desc", str(output), "//omabeam:omabeam-cast", "deps", "--all", "--format=json"],
        cwd=source, text=True))
    deps = graph["//omabeam:omabeam-cast"]["deps"]
    modules = set()
    for dependency in deps:
        path = dependency.split(":")[0].lstrip("/")
        if path.startswith("third_party/"):
            modules.add("/".join(path.split("/")[:2]))
        elif path.startswith("buildtools/third_party/"):
            modules.add("/".join(path.split("/")[:3]))
    inventory = json.loads((NATIVE / "licenses.json").read_text())
    missing = modules - inventory.keys()
    if missing:
        raise RuntimeError(f"Cast dependency license inventory needs review: {sorted(missing)}")
    paths = {"LICENSE", "DEPS"}
    for module in modules:
        paths.update(inventory[module])
        readme = source / module / "README.chromium"
        if readme.is_file():
            paths.add(str(readme.relative_to(source)))
    destination = output.parent.parent.parent / "notices"
    destination.mkdir(exist_ok=True)
    records = []
    for name in sorted(paths):
        path = source / name
        if path.is_symlink() or not path.is_file() or not path.resolve().is_relative_to(source.resolve()):
            raise RuntimeError(f"Missing or unsafe Cast notice: {path}")
        data = path.read_bytes()
        if not data.strip():
            raise RuntimeError(f"Empty Cast notice: {path}")
        target = destination / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(data)
        records.append({"path":name, "sha256":hashlib.sha256(data).hexdigest()})
    manifest = {"upstream":spec, "modules":sorted(modules), "files":records,
                "binary":{"protocol":1, "sha256":hashlib.sha256((output / "omabeam-cast").read_bytes()).hexdigest()}}
    (destination / "manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
    print(f"Cast notices: {destination}")


def pin_sender_inflight(source: Path) -> None:
    """Let the sender hold four 30 fps frames instead of the 66 ms ethernet floor.

    SenderImpl uses max_in_flight_media_duration when it is set. Otherwise the
    window is clamp(2*RTT, 66ms, playout/3), which stays at 66 ms on a fast LAN.
    SenderSession::CreateSender never sets the override. Re-applied after sync
    because gclient restores the upstream file.
    """
    path = source / "cast/streaming/public/sender_session.cc"
    text = path.read_text()
    marker = "config.max_in_flight_media_duration = std::chrono::milliseconds(150);"
    if marker in text:
        return
    anchor = (
        "  OSP_DCHECK(config.IsValid());\n"
        "  return std::make_unique<SenderImpl>(*config_.environment, packet_router_,\n"
        "                                      std::move(config), type);"
    )
    if text.count(anchor) != 1:
        raise RuntimeError("Open Screen SenderSession::CreateSender anchor changed")
    path.write_text(text.replace(anchor,
        "  // OmaBeam: four frames at 30 fps. The default window clamps to 66 ms\n"
        "  // on a low-RTT link, which stalls a receiver that checkpoints late.\n"
        "  config.max_in_flight_media_duration = std::chrono::milliseconds(150);\n"
        "  OSP_DCHECK(config.IsValid());\n"
        "  return std::make_unique<SenderImpl>(*config_.environment, packet_router_,\n"
        "                                      std::move(config), type);",
        1))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache", type=Path, default=ROOT / "target/native-cast")
    parser.add_argument("--sync", action="store_true", help="Fetch pinned dependencies (network required)")
    parser.add_argument("--upstream", action="store_true", help="Also build decoding test receiver and reference sender")
    parser.add_argument("--jobs", type=int, default=4)
    parser.add_argument("--prepare-only", action="store_true", help="Fetch/configure without compiling")
    parser.add_argument("--target-cpu", choices=("x64", "arm64"),
                        help="Linux target CPU (cross-build the production helper on an x86_64 host)")
    options = parser.parse_args()
    if platform.system() not in ("Darwin", "Linux"):
        parser.error("The native Cast helper currently builds on Linux and macOS")
    if platform.system() == "Linux" and platform.machine() != "x86_64":
        parser.error("The pinned Linux build tools require x86_64; build ARM64 there with --target-cpu arm64")
    if options.jobs < 1:
        parser.error("--jobs must be positive")
    if options.target_cpu and (platform.system() != "Linux" or platform.machine() != "x86_64"):
        parser.error("--target-cpu requires a Linux x86_64 build host")
    if options.target_cpu == "arm64" and options.upstream:
        parser.error("Cross-build only the production helper; reference tools link host libraries")
    cache = options.cache.resolve()
    cache.mkdir(parents=True, exist_ok=True)
    spec = json.loads((NATIVE / "upstream.json").read_text())
    depot, source = cache / "depot_tools", cache / "openscreen"
    env = os.environ.copy()
    env.update({"DEPOT_TOOLS_UPDATE": "0", "DEPOT_TOOLS_METRICS": "0",
                "PATH": str(depot) + os.pathsep + env.get("PATH", "")})
    if options.sync:
        checkout(depot, spec["depot_tools"], env)
        checkout(source, spec["openscreen"], env)
        # Use the C++ DNS parser. The helper does not need Chromium's separate
        # Rust toolchain; disabling only that hook saves a substantial download.
        config = {"name": "openscreen", "url": spec["openscreen"]["url"],
                  "managed": False, "custom_deps": {},
                  "custom_vars": {"checkout_clang_coverage_tools": False,
                                  "checkout_instrumented_libraries": False},
                  "custom_hooks": [{"name": "rust_toolchain"}]}
        (cache / ".gclient").write_text("solutions = " + repr([config]) + "\n")
        run([depot / "gclient", "sync", "--no-history", "--jobs", options.jobs], cwd=cache, env=env)
    if not source.is_dir():
        parser.error("Open Screen is missing; run once with --sync")
    revision = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
    if revision != spec["openscreen"]["revision"]:
        parser.error("Cached Open Screen revision differs from upstream.json; run with --sync")
    if options.target_cpu == "arm64" and options.sync:
        # The pinned Clang/GN tools run on x86_64; the pinned target sysroot
        # and GN's clang_arm64 toolchain produce a native ARM64 executable.
        run([sys.executable, source / "build/linux/sysroot_scripts/install-sysroot.py",
             "--arch=arm64"], cwd=source, env=env)
    pin_sender_inflight(source)
    gn = source / "buildtools" / ("mac" if platform.system() == "Darwin" else "linux64") / "gn"
    ninja = source / "third_party/ninja/ninja"
    if not gn.is_file() or not ninja.is_file():
        parser.error("GN/Ninja are missing; run once with --sync")
    # This field is only read by the optional Rust parser. Keep -Werror on
    # while building the supported C++ parser with the pinned LLVM toolchain.
    for filename, kind in [("rtp_packet_parser.h", "RtpParserVersion"),
                           ("compound_rtcp_parser.h", "RtpParserVersion")]:
        header = source / "cast/streaming/impl" / filename
        contents = header.read_text()
        contents = contents.replace(f"  const {kind} version_;",
                                    f"  [[maybe_unused]] const {kind} version_;")
        if header.read_text() != contents:
            header.write_text(contents)
    targets = []
    overlay = NATIVE / "BUILD.gn"
    if overlay.exists():
        dest = source / "omabeam"
        dest.mkdir(exist_ok=True)
        for item in NATIVE.iterdir():
            if item.suffix in (".cc", ".h", ".gn"):
                shutil.copy2(item, dest / item.name)
        # Generate our target from the root, without modifying upstream targets.
        root_build = source / "BUILD.gn"
        marker = "# OmaBeam overlay (scripts/build-cast.py)"
        original = root_build.read_text().split(marker)[0].rstrip()
        root_build.write_text(original + '\n\n' + marker + '\ngroup("omabeam") {\n  deps = [ "//omabeam:omabeam-cast" ]\n}\n')
        # Upstream permits the BoringSSL adapter only to named standalone
        # embedders. Add our production executable to those two allowlists.
        for build_path, anchor in [("cast/common/BUILD.gn", '"../standalone_sender:*",'),
                                   ("platform/BUILD.gn", '"../cast/standalone_sender:*",'),
                                   ("discovery/BUILD.gn", '"../cast/standalone_sender:*",')]:
            build = source / build_path
            contents = build.read_text()
            if '"//omabeam:omabeam-cast"' not in contents:
                if anchor not in contents:
                    raise RuntimeError(f"Upstream visibility anchor changed in {build_path}")
                contents = contents.replace(anchor, anchor + '\n    "//omabeam:omabeam-cast",')
                build.write_text(contents)
        targets.append("omabeam-cast")
    if options.upstream:
        if platform.system() == "Linux":
            reference_startup_objects(source)
        targets.extend(["cast_sender", "cast_receiver"])
    if not targets:
        parser.error("No helper source yet; use --upstream for the feasibility build")
    output = source / "out/omabeam"
    output.mkdir(parents=True, exist_ok=True)
    (output / "args.gn").write_text(gn_arguments(options.upstream, cache / "system-headers", options.target_cpu))
    run([gn, "gen", output], cwd=source, env=env)
    if not options.prepare_only:
        run([ninja, "-C", output, "-j", options.jobs, *targets], cwd=source, env=env)
        if "omabeam-cast" in targets:
            collect_notices(source, output, gn, spec)
            # Do not replace a host helper with a cross-compiled executable.
            for profile in (() if options.target_cpu == "arm64" else ("debug", "release")):
                dest = ROOT / "target" / profile
                if dest.is_dir():
                    shutil.copy2(output / "omabeam-cast", dest / "omabeam-cast")
    print(f"Cast build directory: {output}")


if __name__ == "__main__":
    try:
        main()
    except (OSError, RuntimeError, subprocess.CalledProcessError) as error:
        sys.exit(f"build-cast: {error}")
