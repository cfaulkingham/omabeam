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
  signal started()
  signal exited(int code, int exitStatus)
  function signal(number) { running = false }
  function write(data) {}
  function finish(code, exitStatus, payload) {
    this.output = payload
    running = false
    exited(code, exitStatus)
  }
}''',
            "SplitParser": 'import QtQuick\nQtObject { property string splitMarker: ""; signal read(string chunk) }',
            "StdioCollector": 'import QtQuick\nQtObject { property bool waitForEnd: true; property string text: ""; signal streamFinished() }',
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
    commands = {name: panel.findChild(QObject, name + "Command") for name in ("status", "copy", "stop", "picker", "send")}
    check(engine, panel, 'subject.omabeamBin.endsWith("/omarchy-plugin/omabeam") && subject.omabeamBin.indexOf("file:") < 0')

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
    call('subject.refresh();')
    finish("status", code=1)
    expect('subject.ready && !subject.sessionOn && subject.session.state === "idle"')
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
