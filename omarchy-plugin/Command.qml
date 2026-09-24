import QtQuick
import Quickshell.Io

// One request at a time. SplitParser with an empty marker counts chunks as they
// arrive so a cap can stop the process before the shell holds the whole stream.
Process {
  id: root
  property bool pending: false
  property bool collectOutput: true
  property int timeoutMs: 5000
  property int maxBytes: 8192
  property int maxStderrBytes: 512
  property string output: ""
  property string stdinData: ""
  property bool overflowed: false
  // Diagnostic only: bounded to the last maxStderrBytes so a long-lived
  // command (timeoutMs: 0) cannot grow this without limit.
  property string stderrTail: ""
  signal completed(int code, int exitStatus, string output)
  signal failed(string reason)

  stdinEnabled: false

  // Used by tests/omarchy_ui.py to inject a bounded result.
  function finish(code, exitStatus, payload) {
    output = payload
    overflowed = false
    deadline.stop()
    killTimer.stop()
    running = false
    if (!pending) {
      completed(code, exitStatus, payload)
      return
    }
    pending = false
    completed(code, exitStatus, payload)
  }

  function start(args, input) {
    if (pending || running) return false
    output = ""
    stderrTail = ""
    overflowed = false
    killTimer.stop()
    command = args
    stdinData = input || ""
    stdinEnabled = stdinData !== ""
    pending = true
    if (timeoutMs > 0) deadline.restart()
    running = true
    return true
  }

  function terminate() {
    if (!running) return
    root.signal(15)
    killTimer.restart()
  }

  function fail(reason) {
    if (!pending) return
    pending = false
    deadline.stop()
    terminate()
    failed(reason)
  }

  stdout: SplitParser {
    splitMarker: ""
    onRead: function(chunk) {
      if (!root.collectOutput) return
      root.output += chunk
      if (root.output.length > root.maxBytes) {
        root.overflowed = true
        root.output = ""
        root.fail("OmaBeam returned more data than expected.")
      }
    }
  }
  stderr: SplitParser {
    splitMarker: ""
    onRead: function(chunk) {
      root.stderrTail += chunk
      if (root.stderrTail.length > root.maxStderrBytes)
        root.stderrTail = root.stderrTail.slice(root.stderrTail.length - root.maxStderrBytes)
    }
  }
  onStarted: {
    if (root.stdinData !== "") {
      root.write(root.stdinData)
      root.stdinEnabled = false
      root.stdinData = ""
    }
  }
  onExited: function(code, exitStatus) {
    killTimer.stop()
    deadline.stop()
    if (!root.pending) return
    root.pending = false
    if (root.overflowed) return
    root.completed(code, exitStatus, root.output)
  }
  onRunningChanged: {
    if (!running && pending) Qt.callLater(function() {
      if (!root.running && root.pending) root.fail("Could not start the command.")
    })
  }
  property Timer deadline: Timer {
    interval: root.timeoutMs
    onTriggered: root.fail("The command took too long to respond.")
  }
  property Timer killTimer: Timer {
    interval: 2000
    onTriggered: if (root.running) root.signal(9)
  }
  // A timeoutMs: 0 command is a deliberately long-lived UI launch (the
  // picker, the send window); leaving the panel must not kill it. Only a
  // bounded command still running at destruction gets signaled. Exposed as
  // a function so tests can exercise it without racing Qt's own async
  // object teardown.
  function teardown() {
    if (root.running && root.timeoutMs > 0) {
      root.signal(15)
      root.signal(9)
    }
  }
  Component.onDestruction: teardown()
}
