#!/usr/bin/env python3
"""Opt-in physical Cast capture test for Omarchy/Hyprland's Lua API.

Requires qml6, grim, and a selected receiver ID from --cast-devices. Creates a
synthetic window on a temporary headless output; never selects an existing
window or desktop. Transport counters require separate visual confirmation.
"""
import argparse, json, os, pathlib, signal, subprocess, tempfile, time

ROOT = pathlib.Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('receiver', help='Explicitly selected receiver ID')
parser.add_argument('mode', choices=('window', 'output', 'region', 'extended'))
parser.add_argument('--width', type=int, choices=(1280,1920), default=1280)
parser.add_argument('--seconds', type=float, default=20)
parser.add_argument('--binary', type=pathlib.Path, default=ROOT / 'target/release/omabeam')
parser.add_argument('--output', type=pathlib.Path, default=ROOT / 'target/native-cast/hyprland')
parser.add_argument('--encoder', choices=('auto','software','hardware'), default='auto')
parser.add_argument('--source-only', action='store_true', help='Check the generated window without casting')
args = parser.parse_args()
if not 10 <= args.seconds <= 600: parser.error('--seconds must be between 10 and 600')
if args.source_only and args.mode == 'extended': parser.error('--source-only needs window/output/region mode')
root = args.output.resolve()
root.mkdir(parents=True, exist_ok=True)
binary = args.binary.resolve()
if not args.source_only and not binary.is_file(): parser.error('Build the app before running this test')
receiver, mode, width = args.receiver, args.mode, str(args.width)
source_only, duration = args.source_only, args.seconds
instances = json.loads(subprocess.check_output(['hyprctl','instances','-j']))
selected = os.environ.get('HYPRLAND_INSTANCE_SIGNATURE')
if selected: instances = [instance for instance in instances if instance['instance'] == selected]
if len(instances) != 1: parser.error('Select one Hyprland session with HYPRLAND_INSTANCE_SIGNATURE')
instance = instances[0]
real_runtime = pathlib.Path(os.environ.get('XDG_RUNTIME_DIR', '/run/user/' + str(os.getuid())))
report = root / ('linux-' + mode + '-' + width + '.json')

with tempfile.TemporaryDirectory(prefix='obq-') as temp:
    runtime = pathlib.Path(temp)
    (runtime / 'hypr').symlink_to(real_runtime / 'hypr', target_is_directory=True)
    env = {**os.environ, 'XDG_RUNTIME_DIR':str(runtime),
           'WAYLAND_DISPLAY':str(real_runtime / instance['wl_socket']),
           'HYPRLAND_INSTANCE_SIGNATURE':instance['instance'], 'QT_QPA_PLATFORM':'wayland'}
    def hypr(*args):
        return subprocess.check_output(['hyprctl', *args], env=env, text=True, timeout=10)
    def query(name): return json.loads(hypr(name, '-j'))
    def wait_for(check, seconds=15):
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            value = check()
            if value: return value
            time.sleep(.1)
        raise AssertionError('Condition timed out')
    original = query('monitors')
    original_names = {m['name'] for m in original}
    assert not any(name.startswith('OMABEAM-') for name in original_names), 'Stop the existing extended display before qualification'
    original_focus = next((m['name'] for m in original if m['focused']), None)
    title = 'OmaBeam Cast QA ' + mode
    qml = runtime / 'pattern.qml'
    qml.write_text('''import QtQuick
import QtQuick.Window
Window {
  visible: true; width: 1000; height: 560; color: "#172554"
  title: "''' + title + '''"
  Rectangle { x: 0; width: parent.width / 3; height: parent.height; color: "#ef4444" }
  Rectangle { x: parent.width / 3; width: parent.width / 3; height: parent.height; color: "#22c55e" }
  Rectangle { x: parent.width * 2 / 3; width: parent.width / 3; height: parent.height; color: "#3b82f6" }
  Rectangle { width: 100; height: 100; y: 180; color: "white"
    SequentialAnimation on x { loops: Animation.Infinite
      NumberAnimation { from: 0; to: 850; duration: 1800 }
      NumberAnimation { from: 850; to: 0; duration: 1800 }
    }
  }
  Text { anchors.centerIn: parent; text: "OmaBeam Linux Cast — ''' + mode + '''"; font.pixelSize: 36; color: "black" }
}
''')
    app = window = None
    manual_output = None
    log_path = root / ('linux-' + mode + '-' + width + '.log')
    window_log = root / ('linux-' + mode + '-window.log')
    status = runtime / 'omabeam/live.json'
    try:
        with log_path.open('w') as log, window_log.open('w') as wlog:
            if mode == 'extended':
                source = ['extend', width, str(int(width)*9//16), '1', 'right']
            else:
                manual_output = 'CAST-QA-' + str(os.getpid())
                hypr('output','create','headless',manual_output)
                hypr('eval', 'hl.monitor({output="' + manual_output + '", mode="1280x720@60", position="auto-right", scale=1})')
                wait_for(lambda: any(m['name'] == manual_output for m in query('monitors')))
                source = None
            if mode == 'extended':
                app = subprocess.Popen([str(binary),'--encoder',args.encoder,'--width',width,'--cast',receiver,'--',*source], env=env, stdout=log, stderr=log)
                def created():
                    if app.poll() is not None: raise AssertionError(log_path.read_text()[-4000:])
                    return next((m['name'] for m in query('monitors') if m['name'] not in original_names and m['name'].startswith('OMABEAM-')), None)
                output = wait_for(created, 45)
            else: output = manual_output
            window = subprocess.Popen(['qml6',str(qml)], env=env, stdout=wlog, stderr=wlog)
            client = wait_for(lambda: next((c for c in query('clients') if c['pid'] == window.pid), None))
            address = client['address']
            monitor = next(m for m in query('monitors') if m['name'] == output)
            hypr('dispatch', 'hl.dsp.window.move({ workspace = ' + json.dumps(monitor['activeWorkspace']['name']) + ', window = ' + json.dumps('address:' + address) + ' })')
            output_id = monitor['id']
            wait_for(lambda: any(c['address'] == address and c['monitor'] == output_id for c in query('clients')))
            if original_focus: hypr('dispatch','hl.dsp.focus({ monitor = ' + json.dumps(original_focus) + ' })')
            if source_only:
                time.sleep(1)
                subprocess.run(['grim','-o',output,str(root / ('linux-' + mode + '-source.png'))],env=env,check=True,timeout=10)
                print('PASS isolated generated-window source setup', flush=True)
                raise SystemExit(0)
            if mode != 'extended':
                if mode == 'window': source = ['window', address, client['stableId'], title]
                elif mode == 'output': source = ['output',output]
                elif mode == 'region': source = ['region',output,'0','0','640','360']
                else: raise AssertionError('unknown mode')
                app = subprocess.Popen([str(binary),'--encoder',args.encoder,'--width',width,'--cast',receiver,'--',*source], env=env, stdout=log, stderr=log)
            start = time.monotonic()
            sample = None
            streaming_since = None
            while time.monotonic() - start < duration + 45:
                if app.poll() is not None: raise AssertionError(log_path.read_text()[-5000:])
                if status.exists():
                    sample = json.loads(status.read_text())
                    if sample.get('error'): raise AssertionError(sample['error'])
                    if sample.get('cast',{}).get('connection') == 'streaming':
                        streaming_since = streaming_since or time.monotonic()
                    if (sample.get('cast',{}).get('accepted_frames',0) >= 30
                        and sample['cast'].get('control_heartbeats',0) >= 2
                        and streaming_since and time.monotonic() - streaming_since >= duration): break
                time.sleep(.25)
            assert sample and sample.get('cast',{}).get('control_heartbeats',0) >= 2, 'No sustained capture'
            assert sample['cast'].get('accepted_frames',0) >= 30, 'Too few frames admitted'
            assert streaming_since and time.monotonic() - streaming_since >= duration, 'Streaming duration was too short'
            seconds_observed = round(time.monotonic() - streaming_since, 1)
            subprocess.run(['grim','-o',output,str(root / ('linux-' + mode + '-source.png'))],env=env,check=True,timeout=10)
            app.send_signal(signal.SIGTERM)
            assert app.wait(timeout=7) == 0, log_path.read_text()[-3000:]
            assert not status.exists(), 'status remains'
            if mode == 'extended':
                assert not any(m['name'] == output for m in query('monitors')), 'owned output remains'
            result = {k:sample[k] for k in ('width','height','fps','frames','uptime')}
            result['cast'] = {k:v for k,v in sample['cast'].items() if k not in ('receiver_id','receiver_name','session_id')}
            result.update(mode=mode, synthetic_window=True, real_wayland_capture=True, clean_stop=True, owned_output_removed=mode=='extended', seconds_observed=seconds_observed)
            report.write_text(json.dumps(result,indent=2)+'\n')
            print(json.dumps(result,indent=2),flush=True)
    finally:
        if window is not None and window.poll() is None:
            window.terminate()
            try: window.wait(timeout=5)
            except subprocess.TimeoutExpired: window.kill(); window.wait()
        if app is not None and app.poll() is None:
            app.terminate()
            try: app.wait(timeout=7)
            except subprocess.TimeoutExpired: app.kill(); app.wait()
        if app is not None:
            subprocess.run([str(binary),'--stop'], env=env, capture_output=True, timeout=10)
        if manual_output and any(m['name'] == manual_output for m in query('monitors')):
            hypr('output','remove',manual_output)
            wait_for(lambda: not any(m['name'] == manual_output for m in query('monitors')))
        if original_focus: hypr('dispatch','hl.dsp.focus({ monitor = ' + json.dumps(original_focus) + ' })')
