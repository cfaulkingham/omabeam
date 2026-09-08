import QtQuick
import Quickshell.Io

// One request at a time; collection is reset for every invocation. Quickshell
// emits runningChanged (but not exited) when an executable cannot be started.
Process {
  id: root
  property bool pending: false
  property int timeoutMs: 5000
  property string output: ""
  signal completed(int code, int exitStatus, string output)
  signal failed(string reason)

  function start(args) {
    if (pending || running) return false
    output = ""
    command = args
    pending = true
    deadline.restart()
    running = true
    return true
  }

  function fail(reason) {
    if (!pending) return
    pending = false
    deadline.stop()
    if (running) root.signal(9)
    failed(reason)
  }

  stdout: StdioCollector {
    waitForEnd: true
    onStreamFinished: root.output = text
  }
  onExited: function(code, exitStatus) {
    if (!root.pending) return
    root.pending = false
    deadline.stop()
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
}
