#!/usr/bin/env python3
"""Offscreen QML QA. Real plugin code; shell/process boundaries are simulated.

Install PySide6-Essentials, then run: python tests/omarchy_ui.py
Pass --screenshots DIRECTORY to save the rendered visual states.
This cannot validate Hyprland's layer-shell integration or the Wayland clipboard.
"""
import argparse
import json
import os
from pathlib import Path
import tempfile

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")
os.environ.setdefault("QT_QUICK_BACKEND", "software")

from PySide6.QtCore import QObject, QPoint, Qt, QUrl, qInstallMessageHandler
from PySide6.QtGui import QGuiApplication, QFont, QFontDatabase
from PySide6.QtQml import QJSEngine, QQmlComponent, QQmlEngine, QQmlExpression
from PySide6.QtQuick import QQuickView
from PySide6.QtTest import QTest

ROOT = Path(__file__).resolve().parents[1]
LIVE = dict(pid=1234, state="live", title="Terminal — OmaBeam development",
            source="Terminal — OmaBeam development",
            url="http://192.168.1.24:9847/s/0123456789abcdef0123456789abcdef/",
            viewers=2, uptime=754, width=1920, height=1080, fps=14.8)


def evaluate(engine, obj, expression):
    engine.rootContext().setContextProperty("subject", obj)
    exp = QQmlExpression(engine.rootContext(), obj, expression)
    value, _ = exp.evaluate()
    assert not exp.hasError(), exp.error().toString()
    return value


def run(engine, obj, code):
    return evaluate(engine, obj, "(function() { " + code + " })()")


def check(engine, obj, expression):
    assert evaluate(engine, obj, expression), expression


def fixtures(path):
    # Only the imported shell and subprocess APIs are fake. Panel.qml,
    # Command.qml, Session.js, buttons, and all presentation run unchanged.
    modules = {
        "Quickshell": {
            "Quickshell": 'pragma Singleton\nimport QtQuick\nQtObject { function env(name) { return "/test-home" } }',
        },
        "Quickshell/Io": {
            "Process": '''import QtQuick
QtObject {
  property bool running: false
  property bool stdinEnabled: false
  property bool clearEnvironment: false
  property var environment: ({})
  property var command: []
  property var stdout: null
  property var stderr: null
  property string output: ""
  // Records every signal() call so tests can verify teardown behavior.
  property var signals: []
  signal started()
  signal exited(int code, int exitStatus)
  function signal(number) { signals.push(number); running = false }
  function write(data) {}
  function finish(code, exitStatus, payload) {
    this.output = payload
    running = false
    exited(code, exitStatus)
  }
}''',
            "SplitParser": 'import QtQuick\nQtObject { property string splitMarker: ""; signal read(string chunk) }',
            "StdioCollector": 'import QtQuick\nQtObject { property bool waitForEnd: true; property string text: ""; signal streamFinished() }',
            "FileView": '''import QtQuick
QtObject {
  property string path: ""
  property bool watchChanges: false
  property bool preload: true
  property bool printErrors: true
  signal loaded()
  signal loadFailed(string error)
  signal fileChanged()
}''',
        },
        "qs/Commons": {
            "Color": '''pragma Singleton
import QtQuick
QtObject {
  property color foreground: "#d6deeb"
  property color background: "#161b24"
  property color accent: "#a5c9ad"
  property color urgent: "#e49393"
  property QtObject popups: QtObject { property color text: "#d6deeb"; property color background: "#161b24" }
}''',
            "Style": '''pragma Singleton
import QtQuick
QtObject {
  function space(n) { return n }
  property real cornerRadius: 6
  property QtObject font: QtObject { property string family: "sans-serif"; property int body: 14; property int caption: 12 }
}''',
        },
        "qs/Ui": {
            "Panel": '''import QtQuick
Item {
  property var bar: null
  property var settings: ({})
  property string moduleName: ""
  property bool manageIpc: true
  property bool popoutSwitchClosing: false
  readonly property bool opened: controller.open
  property QtObject controller: QtObject {
    property bool open: false
    function show() { open = true }
    function hide() { open = false }
  }
  function closeForPopoutSwitch() { close() }
}''',
            "KeyboardPanel": '''import QtQuick
Item {
  property var anchorItem: null
  property var owner: null
  property var bar: null
  property bool open: false
  property bool centerOnBar: false
  property var focusTarget: null
  property real padding: 0
  property real contentWidth: 400
  property real contentHeight: 500
  width: contentWidth - padding * 2
  height: contentHeight - padding * 2
  function fittedContentWidth(w) { return w }
  function fittedContentHeight(h) { return h + padding * 2 }
}''',
            "PanelKeyCatcher": '''import QtQuick
Item {
  signal closeRequested()
  signal tabRequested(int direction)
  signal moveRequested(int dx, int dy)
  signal activateRequested()
  signal textKey(string text)
}''',
            "BarWidget": 'import QtQuick\nItem { property var bar: null; property var settings: ({}); property string moduleName: "" }',
            "BarIconButton": '''import QtQuick
Item {
  property var bar: null
  property string text: ""
  property bool active: false
  property bool dimmed: false
  property string tooltipText: ""
  property color foreground: "#d6deeb"
  property Component iconComponent: null
  signal pressed(int buttonCode)
  implicitWidth: 24
  implicitHeight: 24
}''',
        },
    }
    for module, files in modules.items():
        folder = path / module
        folder.mkdir(parents=True)
        lines = ["module " + module.replace("/", ".")]
        for name, code in files.items():
            code = code.replace('"sans-serif"', json.dumps(QGuiApplication.font().family()))
            (folder / (name + ".qml")).write_text(code)
            lines.append(("singleton " if code.startswith("pragma Singleton") else "") + f"{name} 1.0 {name}.qml")
        (folder / "qmldir").write_text("\n".join(lines))


def model_tests():
    engine = QJSEngine()
    result = engine.evaluate((ROOT / "omarchy-plugin/Session.js").read_text().replace(".pragma library", ""))
    assert not result.isError(), result.toString()
    result = engine.evaluate('''
      function assert(value) { if (!value) throw new Error("Session model assertion failed") }
      assert(elapsed(null) === "—" && elapsed(59) === "0:59" && elapsed(60) === "1:00")
      assert(elapsed(3600) === "1:00:00" && elapsed(36001) === "10:00:01")
      const token = "0123456789abcdef0123456789abcdef"
      assert(linkHost("http://[::1]:9847/s/" + token + "/") === "[::1]:9847")
      assert(linkHost("http://192.168.1.24:9847/s/" + token + "/") === "192.168.1.24:9847")
      assert(linkHost("https://example.com/s/" + token + "/") === "")
      assert(linkHost("http://8.8.8.8:9847/s/" + token + "/") === "")
      assert(plain("<img src=x>", 80) === "img src=x")
      const casting = {pid:12,state:"live",title:"Screen",url:"",cast:{session_id:token,
        receiver_id:"tv-id",receiver_name:"Living Room",connection:"negotiating"}}
      const cast = read(JSON.stringify(casting), 0, 0)
      assert(cast.destination === "cast" && cast.receiver === "Living Room" && cast.url === "")
      casting.cast.connection = "streaming"
      assert(read(JSON.stringify(casting), 0, 0).connection === "streaming")
      casting.cast.session_id = "bad"
      let badCast = false
      try { read(JSON.stringify(casting), 0, 0) } catch (error) { badCast = true }
      assert(badCast)
      const url = "http://192.168.1.24:9847/s/" + token + "/"
      const rows = Array(21).fill("1".repeat(21))
      assert(readQr(JSON.stringify({url, rows}), url).length === 21)
      for (const bad of ["{bad", "x".repeat(8193), JSON.stringify({url, rows: []}),
          JSON.stringify({url, rows: Array(22).fill("1".repeat(22))}),
          JSON.stringify({url, rows: Array(21).fill("x".repeat(21))}),
          JSON.stringify({url, rows: Array(85).fill("1".repeat(85))}),
          JSON.stringify({url: url + "other", rows})]) assert(readQr(bad, url).length === 0)
      for (const url of ["file:///tmp/test", "javascript:alert(1)", "http://user:pass@host/",
          "http://host:99999/", "http://host:0/", "http://host/has a space",
          "http://192.168.1.24:9847/s/abc/"])
        assert(linkHost(url) === "")
      for (const data of [null, [], {}, {pid: -1}, {pid: "12"}, {pid: 1.5}, {pid: 1}, {pid: 1, state: "paused"}]) {
        let rejected = false
        try { read(JSON.stringify(data), 0, 0) } catch (error) { rejected = true }
        assert(rejected)
      }
    ''')
    assert not result.isError(), result.toString()
    print("PASS: malformed status, duration boundaries, and browser URL validation")


def controller_tests(app, imports):
    engine = QQmlEngine()
    engine.addImportPath(str(imports))
    component = QQmlComponent(engine, QUrl.fromLocalFile(str(ROOT / "omarchy-plugin/Panel.qml")))
    panel = component.create()
    assert panel, "\n".join(e.toString() for e in component.errors())
    commands = {name: panel.findChild(QObject, name + "Command") for name in ("status", "copy", "stop", "picker", "send", "qr")}
    poll_timer = panel.findChild(QObject, "pollTimer")
    watch_debounce_timer = panel.findChild(QObject, "watchDebounceTimer")
    runtime_watch = panel.findChild(QObject, "runtimeWatch")
    check(engine, panel, 'subject.omabeamBin.endsWith("/omarchy-plugin/omabeam") && subject.omabeamBin.indexOf("file:") < 0')
    check(engine, runtime_watch, 'subject.path === "" && subject.watchChanges === true && subject.printErrors === false')

    def call(code):
        return run(engine, panel, code)

    def expect(code):
        check(engine, panel, code)

    def finish(name, data="", code=0, status=0):
        raw = json.dumps(data) if isinstance(data, dict) else data
        run(engine, commands[name], f"subject.finish({code}, {status}, {json.dumps(raw)});")
        app.processEvents()

    finish("status", code=127)
    expect('subject.ready && subject.statusError.indexOf("--backend-only") >= 0')
    # The runtime directory does not exist before the first --status call
    # (native code creates it as a side effect of that call); the watch is
    # armed right after, success or not, and never re-armed after that.
    check(engine, runtime_watch, 'subject.path === "/test-home/omabeam"')

    # A failed first --status call (a Command-level failure such as a
    # timeout, not just a nonzero exit) must arm the watch too: arming
    # never retries, so this is the only chance a fresh panel gets.
    other_component = QQmlComponent(engine, QUrl.fromLocalFile(str(ROOT / "omarchy-plugin/Panel.qml")))
    other_panel = other_component.create()
    assert other_panel, "\n".join(e.toString() for e in other_component.errors())
    other_status = other_panel.findChild(QObject, "statusCommand")
    other_watch = other_panel.findChild(QObject, "runtimeWatch")
    check(engine, other_watch, 'subject.path === ""')
    run(engine, other_status, 'subject.fail("boom");')
    app.processEvents()
    check(engine, other_watch, 'subject.path === "/test-home/omabeam"')
    other_panel.deleteLater()
    app.processEvents()

    call('subject.refresh();')
    finish("status", code=1)
    expect('subject.ready && !subject.sessionOn && subject.session.state === "idle"')

    # Idle polling backs off to the 30s safety net now that a directory
    # watch handles the common case; open/live cadences are unchanged.
    check(engine, poll_timer, 'subject.interval === 30000')
    call('subject.open();')  # open() also calls refresh() itself
    check(engine, poll_timer, 'subject.interval === 1000')
    finish("status", code=1)
    call('subject.close();')
    check(engine, poll_timer, 'subject.interval === 30000')

    # A SUSTAINED burst of directory-change events while idle (e.g. another
    # bar instance's share rewriting live.json roughly every 250ms, each
    # rename surfacing as 2+ events) must not suppress the refresh for as
    # long as it lasts: the timer starts on the first event and later
    # events do not restart it, so it still fires ~300ms after the FIRST
    # one. 100ms spacing for 2s (well past the old restart()-forever bug's
    # failure point) stays well under the 300ms window between events.
    check(engine, watch_debounce_timer, 'subject.interval === 300 && subject.repeat === false')
    check(engine, commands["status"], '!subject.pending')
    for _ in range(4):
        run(engine, runtime_watch, 'subject.fileChanged(); subject.fileChanged();')
        QTest.qWait(100)
    check(engine, commands["status"], 'subject.pending')  # fired within ~400ms of the FIRST event
    for _ in range(16):
        run(engine, runtime_watch, 'subject.fileChanged(); subject.fileChanged();')
        QTest.qWait(100)
    check(engine, commands["status"], 'subject.pending')  # still just the one in-flight request
    run(engine, watch_debounce_timer, 'subject.stop();')  # clean slate for the next phase below
    finish("status", code=1)

    # While sharing, or while the panel is open, watch events are ignored
    # outright: the 2s/1s poll already covers those states, so reacting
    # here would only add extra --status spawns on top of it.
    call('subject.refresh();')
    finish("status", LIVE)
    check(engine, commands["status"], '!subject.pending')
    run(engine, runtime_watch, 'subject.fileChanged();')
    QTest.qWait(350)
    check(engine, commands["status"], '!subject.pending')  # ignored: sessionOn
    call('subject.refresh();')
    finish("status", code=1)
    call('subject.open();')  # open() calls refresh() itself; drain it first
    finish("status", code=1)
    run(engine, runtime_watch, 'subject.fileChanged();')
    QTest.qWait(350)
    check(engine, commands["status"], '!subject.pending')  # ignored: opened, even while idle
    call('subject.close();')
    print("PASS: directory watch arms after the first status call (success or failure), idle cadence backs off, a sustained burst still debounces to one refresh while idle, and is ignored while sharing or open")
    call('subject.open(); subject.launchPicker();')
    expect('subject.opened')  # Do not hide a failed launch.
    run(engine, commands["picker"], 'subject.running = false;')
    app.processEvents()
    expect('subject.opened && subject.feedbackError')
    call('subject.launchPicker();')
    run(engine, commands["picker"], 'subject.started();')
    expect('!subject.opened')
    finish("picker")
    finish("status", LIVE)
    expect('subject.sessionOn && subject.session.viewers === 2')
    call('subject.sendNearby(); subject.sendNearby();')
    check(engine, commands["send"], 'subject.command[0].indexOf("omabeam") >= 0 && subject.command[1] === "--send-link" && subject.command.length === 2 && subject.pending')
    run(engine, commands["send"], 'subject.started();')
    expect('!subject.opened')
    finish("send")
    finish("status", LIVE)
    call('subject.open(); subject.copyUrl(); subject.copyUrl();')
    check(engine, commands["copy"], 'subject.command[0] === "/usr/bin/wl-copy" && subject.command[1] === "--" && subject.command.length === 2 && subject.pending')
    expect('subject.feedback !== "Share link copied."')
    finish("copy", code=1)
    expect('subject.feedbackError && subject.feedback.indexOf("Could not copy") === 0')
    call('subject.copyUrl();')
    finish("copy")
    expect('!subject.feedbackError && subject.feedback === "Share link copied."')

    # QR generation is explicit and never places the capability URL in argv.
    rows = ["1" * 21] * 21
    call('subject.toggleQr(); subject.toggleQr();')
    expect('!subject.qrVisible')
    finish("qr", dict(url=LIVE["url"], rows=rows))
    expect('subject.qrRows.length === 0')  # Closed requests cannot reveal a code.
    call('subject.toggleQr();')
    check(engine, commands["qr"], 'subject.command.length === 2 && subject.command[1] === "--share-qr" && subject.stdinData === ""')
    finish("qr", dict(url=LIVE["url"], rows=rows))
    expect('subject.qrVisible && subject.qrRows.length === 21 && subject.qrError === ""')
    call('subject.close();')
    expect('!subject.qrVisible && subject.qrRows.length === 0 && subject.qrUrl === ""')
    call('subject.open(); subject.toggleQr();')
    changed = dict(LIVE, url=LIVE["url"].replace("0123456789abcdef", "abcdef0123456789"))
    finish("status", changed)
    finish("qr", dict(url=LIVE["url"], rows=rows))
    expect('!subject.qrVisible && subject.qrRows.length === 0')
    call('subject.toggleQr();')
    finish("qr", dict(url=LIVE["url"], rows=rows))
    expect('subject.qrVisible && subject.qrRows.length === 0 && subject.qrError !== ""')
    call('subject.close(); subject.refresh();')
    finish("status", LIVE)

    # Malformed output must not erase a known live session or expose stale links.
    call('subject.refresh();')
    finish("status", "{bad json")
    expect('subject.sessionOn && subject.statusError !== ""')
    call('subject.copyUrl();')
    check(engine, commands["copy"], '!subject.pending')
    call('subject.refresh();')
    finish("status", LIVE)

    # An in-flight status read cannot undo the pending stop. Success waits for
    # a NEW status read after --stop exits, rather than the exit code alone.
    call('subject.refresh(); subject.stopLive();')
    finish("status", LIVE)
    expect('subject.sessionOn && subject.stopPending')
    finish("stop")
    expect('subject.sessionOn && subject.stopPending')
    finish("status", code=1)
    expect('!subject.sessionOn && !subject.stopPending && subject.feedback === "Sharing stopped."')

    call('subject.refresh();')
    finish("status", LIVE)
    call('subject.stopLive();')
    finish("stop")
    finish("status", LIVE)
    expect('subject.sessionOn && !subject.stopPending && subject.feedbackError')
    # Missing executables and timeouts have no process exit signal.
    call('subject.refresh();')
    run(engine, commands["status"], 'subject.running = false;')
    app.processEvents()
    expect('subject.sessionOn && subject.statusError !== ""')
    call('subject.refresh();')
    finish("status", LIVE)
    run(engine, commands["copy"], 'subject.timeoutMs = 1;')
    call('subject.copyUrl();')
    QTest.qWait(30)
    check(engine, commands["copy"], '!subject.pending && !subject.running')
    expect('subject.feedbackError')

    call('subject.refresh();')
    finish("status", dict(LIVE, state="ended", error="The shared window was closed."))
    expect('!subject.sessionOn && subject.sessionProblem && subject.session.error.indexOf("closed") > 0')
    call('subject.refresh();')
    finish("status", dict(pid=4321, url=LIVE["url"], title="Incomplete session"))
    expect('!subject.sessionOn && subject.statusError !== ""')
    call('subject.refresh();')
    finish("status", dict(LIVE, url="file:///etc/passwd"))
    expect('subject.sessionOn && subject.session.url === ""')
    call('subject.refresh();')
    finish("status", dict(LIVE, url="https://example.com/s/0123456789abcdef0123456789abcdef/"))
    expect('subject.sessionOn && subject.session.url === ""')
    call('subject.refresh();')
    finish("status", dict(LIVE, title="x" * 201))
    expect('subject.sessionOn && subject.statusError !== "" && subject.session.title.indexOf("Terminal") === 0')
    call('subject.refresh();')
    finish("status", code=1, status=1)
    expect('subject.sessionOn && subject.statusError !== ""')

    # Destroying the panel must not signal a timeoutMs: 0 launch (the picker,
    # the send window) but still stops a bounded command left running.
    # teardown() is exercised directly rather than via real object
    # destruction, which races Qt's own deferred deletion in a test.
    run(engine, commands["send"], 'subject.running = true;')
    run(engine, commands["picker"], 'subject.running = true;')
    run(engine, commands["stop"], 'subject.running = true;')
    run(engine, commands["send"], 'subject.teardown();')
    run(engine, commands["picker"], 'subject.teardown();')
    run(engine, commands["stop"], 'subject.teardown();')
    check(engine, commands["send"], 'subject.signals.length === 0')
    check(engine, commands["picker"], 'subject.signals.length === 0')
    check(engine, commands["stop"], 'subject.signals.length === 2 && subject.signals[0] === 15 && subject.signals[1] === 9')
    run(engine, commands["send"], 'subject.running = false;')
    run(engine, commands["picker"], 'subject.running = false;')
    run(engine, commands["stop"], 'subject.running = false;')

    # A stop timeout is not proof the share stopped: no error appears until
    # the status read it triggers confirms the outcome either way. The tail
    # is bounded to the most recent bytes, and appears sanitized in feedback.
    call('subject.refresh();')
    finish("status", LIVE)
    call('subject.stopLive();')
    run(engine, commands["stop"], "subject.stderr.read('a'.repeat(400));")
    run(engine, commands["stop"], "subject.stderr.read('b'.repeat(600));")
    check(engine, commands["stop"], 'subject.stderrTail.length === 512 && subject.stderrTail.indexOf("a") < 0 && subject.stderrTail.indexOf("b") >= 0')
    run(engine, commands["stop"], 'subject.fail("The command took too long to respond.");')
    expect('subject.sessionOn && subject.stopPending && subject.feedback === ""')
    finish("status", code=1)
    expect('!subject.sessionOn && !subject.stopPending && subject.feedback === "Sharing stopped." && !subject.feedbackError')

    call('subject.refresh();')
    finish("status", LIVE)
    call('subject.stopLive();')
    run(engine, commands["stop"], "subject.stderr.read('permission denied: <no-tty>');")
    finish("stop", code=1)
    expect('subject.feedbackError && subject.feedback.indexOf("permission denied: no-tty") >= 0 && subject.feedback.indexOf("<") < 0')
    call('subject.refresh();')
    finish("status", LIVE)
    print("PASS: destruction spares long-lived launches, stop timeout defers to status, stderr tail is bounded and sanitized")

    widget_component = QQmlComponent(engine, QUrl.fromLocalFile(str(ROOT / "omarchy-plugin/BarWidget.qml")))
    widget = widget_component.create()
    assert widget, "\n".join(e.toString() for e in widget_component.errors())
    app.processEvents()
    widget.deleteLater()
    panel.deleteLater()
    app.processEvents()
    print("PASS: status validation, failed copy/launch, timeout, stale reads, stop verification, and bar loading")


def presentation_tests(app, screenshots):
    view = QQuickView()
    view.engine().rootContext().setContextProperty("previewFontFamily", QGuiApplication.font().family())
    view.setSource(QUrl.fromLocalFile(str(ROOT / "tests/omarchy-preview.qml")))
    assert not view.errors(), "\n".join(e.toString() for e in view.errors())
    view.show()
    root = view.rootObject()
    engine = view.engine()
    run(engine, root, 'subject.content.session = ' + json.dumps(LIVE) + ';')
    run(engine, root, 'subject.content.forceActiveFocus();')
    QTest.qWait(50)
    app.processEvents()
    QTest.keyClick(view, Qt.Key_Return)
    check(engine, root, 'subject.copies === 1')
    QTest.keyClick(view, Qt.Key_Down)
    QTest.keyClick(view, Qt.Key_Return)
    check(engine, root, 'subject.qrRequests === 1')
    QTest.keyClick(view, Qt.Key_Down)
    QTest.keyClick(view, Qt.Key_Return)
    check(engine, root, 'subject.viewers === 1')
    QTest.keyClick(view, Qt.Key_Down)
    QTest.keyClick(view, Qt.Key_Return)
    check(engine, root, 'subject.sends === 1')
    QTest.keyClick(view, Qt.Key_Down)
    QTest.keyClick(view, Qt.Key_Return)
    check(engine, root, 'subject.stops === 1')

    def capture(name, session=None, extra=""):
        run(engine, root, 'subject.width = 400; subject.height = Qt.binding(function() { return subject.content.implicitHeight + 36 });'
            'subject.content.statusError = ""; subject.content.stopping = false; subject.content.feedback = "";'
            'subject.content.background = "#161b24"; subject.content.foreground = "#d6deeb"; subject.content.accent = "#a5c9ad";'
            'subject.content.urgent = "#e49393";'
            'subject.content.session = ' + json.dumps(session or LIVE) + '; subject.content.resetCursor();' + extra)
        QTest.qWait(40)
        assert root.height() > 0
        if screenshots:
            image = view.grabWindow()
            assert not image.isNull(), "Offscreen rendering failed"
            assert image.save(str(screenshots / (name + ".png")))

    capture("live")
    content = root.findChild(QObject, "shareContent")
    scroller = content.findChild(QObject, "detailsScroller")
    assert scroller.property("contentHeight") <= scroller.height() + 1, "Natural size must fit its details"
    run(engine, root, 'subject.content.copying = true;')
    QTest.keyClick(view, Qt.Key_Up)
    check(engine, root, 'subject.content.selectedAction === "stop"')
    run(engine, root, 'subject.content.copying = false;')
    capture("waiting", dict(LIVE, viewers=0, uptime=4))
    capture("ready", dict(state="idle", title="", url=""))
    QTest.keyClick(view, Qt.Key_Return)
    check(engine, root, 'subject.launches === 1 && subject.stops === 1')
    capture("ended", dict(LIVE, state="ended", error="The shared window was closed. Choose a source to start a new share."))
    capture("unavailable", extra='subject.content.statusError = "Could not check OmaBeam. Check that it is installed.";')
    QTest.keyClick(view, Qt.Key_Return)
    check(engine, root, 'subject.refreshes === 1')
    capture("stopping", extra='subject.content.stopping = true;')
    QTest.keyClick(view, Qt.Key_Return)
    check(engine, root, 'subject.copies === 1 && subject.stops === 1')
    capture("light", extra='subject.content.background = "#f5f3ed"; subject.content.foreground = "#282c34"; subject.content.accent = "#315b48"; subject.content.urgent = "#a13636";')
    capture("compact", dict(LIVE, title="<b>A very long window title</b> " * 6, uptime=36001),
            'subject.width = 320; subject.height = 400;')
    stop = content.findChild(QObject, "stop")
    point = stop.mapToScene(QPoint(0, 0))
    assert point.y() >= 0 and point.y() + stop.height() <= root.height(), "Stop must remain visible"
    scroller = content.findChild(QObject, "detailsScroller")
    assert scroller.property("contentHeight") > scroller.height(), "Small layouts must scroll"
    # A real pointer click on Stop remains usable after the details overflow.
    QTest.mouseClick(view, Qt.LeftButton, pos=QPoint(int(point.x() + stop.width() / 2), int(point.y() + stop.height() / 2)))
    check(engine, root, 'subject.stops === 2')
    cast_session = dict(LIVE, destination="cast", receiver="Living Room TV",
                        connection="negotiating", url="", viewers=0, width=1280, height=720)
    capture("cast-connecting", cast_session)
    check(engine, root, 'subject.content.connecting && !subject.content.canShare && subject.content.actions().length === 1 && subject.content.selectedAction === "stop"')
    capture("cast-streaming", dict(cast_session, connection="streaming", viewers=1))
    check(engine, root, '!subject.content.connecting && subject.content.heading === "Casting to Living Room TV"')
    QTest.keyClick(view, Qt.Key_Return)
    check(engine, root, 'subject.stops === 3 && subject.copies === 1')
    capture("cast-reconnecting", dict(cast_session, connection="reconnecting"))
    check(engine, root, 'subject.content.reconnecting && subject.content.heading === "Reconnecting to Living Room TV" && subject.content.actions().length === 1')
    view.close()
    print("PASS: keyboard and pointer actions, ready/live/waiting/ended/error states, light theme, compact scrolling")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--screenshots", type=Path)
    args = parser.parse_args()
    if args.screenshots:
        args.screenshots.mkdir(parents=True, exist_ok=True)
    app = QGuiApplication([])
    families = QFontDatabase.families()
    app.setFont(QFont(next(name for name in ("DejaVu Sans", "Noto Sans", "Helvetica Neue", "Arial") if name in families)))
    warnings = []
    qInstallMessageHandler(lambda level, context, message: warnings.append(message))
    model_tests()
    with tempfile.TemporaryDirectory(prefix="omabeam-qml-") as folder:
        imports = Path(folder)
        fixtures(imports)
        controller_tests(app, imports)
    presentation_tests(app, args.screenshots)
    assert not warnings, "\n".join(warnings)


if __name__ == "__main__":
    main()
