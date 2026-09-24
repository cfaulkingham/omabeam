import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui as Shell
import "Session.js" as Session

Shell.Panel {
  id: root
  moduleName: "io.github.cfaulkingham.omabeam"
  manageIpc: false

  property var anchorItem: null
  property var hostWidget: null
  property var session: Session.empty()
  property bool ready: false
  property string statusError: ""
  property string feedback: ""
  property bool feedbackError: false
  property bool stopPending: false
  property int statusEpoch: 0
  property int pollEpoch: 0
  property bool runtimeWatchArmed: false
  property string copiedUrl: ""
  property string qrUrl: ""
  property var qrRows: []
  property string qrError: ""
  property bool qrVisible: false

  function clearQr() {
    qrVisible = false
    qrRows = []
    qrUrl = ""
    qrError = ""
    qrCommand.output = ""
  }
  function toggleQr() {
    if (qrVisible) { clearQr(); return }
    if (!content.canShare || qrCommand.pending || qrCommand.running || !Session.parseShareUrl(session.url)) return
    qrVisible = true
    qrUrl = session.url
    qrRows = []
    qrError = ""
    qrCommand.start([omabeamBin, "--share-qr"])
  }

  readonly property var barIdentity: hostWidget || root
  readonly property bool sessionOn: session.state === "live"
  readonly property bool sessionProblem: statusError !== "" || session.state === "ended"
  readonly property string sessionSummary: Session.plain(statusError ? "OmaBeam · status unavailable"
    : !ready ? "OmaBeam · checking status"
    : sessionOn ? "Sharing " + session.title + (session.viewers === null ? ""
      : " · " + session.viewers + (session.viewers === 1 ? " viewer" : " viewers"))
    : session.state === "ended" ? "Share ended · " + (session.error || session.title)
    : "OmaBeam · choose a window, screen, or area")
  readonly property string omabeamBin: decodeURIComponent(String(Qt.resolvedUrl("omabeam")).replace(/^file:\/\//, ""))

  function open() {
    content.resetCursor()
    root.refresh()
    root.controller.show()
    Qt.callLater(function() { if (root.opened) root.setCenterHoverRevealSuppressed(true) })
  }
  function close() {
    clearQr()
    root.setCenterHoverRevealSuppressed(false)
    root.controller.hide()
  }
  function toggle() { root.opened ? root.close() : root.open() }
  function setCenterHoverRevealSuppressed(value) {
    if (root.bar && typeof root.bar.setCenterHoverRevealSuppressed === "function")
      root.bar.setCenterHoverRevealSuppressed(value)
    else if (root.bar && "centerHoverRevealSuppressed" in root.bar)
      root.bar.centerHoverRevealSuppressed = value
  }
  function switchPanel(direction) {
    if (root.bar && typeof root.bar.switchPanelFrom === "function")
      return root.bar.switchPanelFrom(root.barIdentity, direction)
    return false
  }
  function syncHost() {
    if (!hostWidget) return
    hostWidget.sessionOn = sessionOn
    hostWidget.sessionProblem = sessionProblem
    hostWidget.sessionSummary = sessionSummary
  }
  onSessionOnChanged: syncHost()
  onSessionProblemChanged: syncHost()
  onSessionSummaryChanged: syncHost()
  onHostWidgetChanged: syncHost()
  Component.onCompleted: refresh()
  Component.onDestruction: setCenterHoverRevealSuppressed(false)

  function showFeedback(message, error) {
    feedback = message
    feedbackError = error === true
    feedbackTimer.restart()
  }
  // Folds a command's captured stderr tail into a user-facing message, e.g.
  // "Could not stop sharing. (permission denied)". Empty when there was none.
  function withCause(message, tail) {
    var cause = Session.plain(tail)
    return cause ? message + " (" + cause + ")" : message
  }
  // live.json's directory does not exist until the first --status call has
  // run (which creates it as a side effect, live share or not), so the
  // watch can only be armed once that call has completed.
  function armRuntimeWatch() {
    if (runtimeWatchArmed) return
    runtimeWatchArmed = true
    runtimeWatch.path = (Quickshell.env("XDG_RUNTIME_DIR") || "") + "/omabeam"
  }
  function refresh() {
    if (statusCommand.pending || stopCommand.pending) return
    pollEpoch = statusEpoch
    statusCommand.start([omabeamBin, "--status"])
  }
  function statusFailed(reason) {
    clearQr()
    ready = true
    statusError = reason
    if (stopPending) {
      stopPending = false
      showFeedback("Could not confirm the share stopped. Check again before leaving.", true)
    }
  }
  function receiveStatus(raw, code, exitStatus, stderrTail) {
    if (pollEpoch !== statusEpoch) { Qt.callLater(root.refresh); return }
    if (code === 127) {
      statusFailed("OmaBeam needs its native app. Run ./install.sh --backend-only in the plugin folder, then check status again.")
      return
    }
    var next
    try { next = Session.read(raw, code, exitStatus) }
    catch (error) { statusFailed(withCause(error.message, stderrTail)); return }
    if (next.url !== session.url || next.state !== session.state) {
      clearQr()
      feedback = ""
      feedbackTimer.stop()
    }
    session = next
    ready = true
    statusError = ""
    if (stopPending) {
      stopPending = false
      showFeedback(sessionOn ? "Still sharing. Try stopping again." : "Sharing stopped.", sessionOn)
    }
  }
  function copyUrl() {
    if (!content.canShare || copyCommand.pending) return
    if (!Session.parseShareUrl(session.url)) return
    copiedUrl = session.url
    copyCommand.clearEnvironment = true
    copyCommand.environment = {
      PATH: "/usr/bin:/bin",
      HOME: Quickshell.env("HOME") || "",
      XDG_RUNTIME_DIR: Quickshell.env("XDG_RUNTIME_DIR") || "",
      WAYLAND_DISPLAY: Quickshell.env("WAYLAND_DISPLAY") || "",
      LANG: "C"
    }
    copyCommand.start(["/usr/bin/wl-copy", "--"], copiedUrl)
  }
  function openViewer() {
    if (!content.canShare || !Session.parseShareUrl(session.url)) return
    if (!Qt.openUrlExternally(session.url)) showFeedback("Could not open your browser. Copy the link instead.", true)
  }
  function sendNearby() {
    if (!content.canShare || sendCommand.pending) return
    if (!Session.parseShareUrl(session.url)) return
    sendCommand.start([omabeamBin, "--send-link"])
  }
  function stopLive() {
    if (!sessionOn || stopPending) return
    // Ignore any status request started before the stop command.
    statusEpoch += 1
    clearQr()
    stopPending = true
    feedback = ""
    stopCommand.start([omabeamBin, "--stop"])
  }
  function launchPicker() {
    if (!ready || sessionOn || statusError || pickerCommand.pending) return
    pickerCommand.start([omabeamBin])
  }

  Timer {
    // Event-driven refresh (below) handles most changes; idle cadence is
    // just a safety net now, so it can back off from every 5s to every 30s.
    objectName: "pollTimer"
    interval: root.opened ? 1000 : root.sessionOn ? 2000 : 30000
    running: true
    repeat: true
    onTriggered: root.refresh()
  }
  Timer { id: feedbackTimer; interval: root.feedbackError ? 7000 : 3000; onTriggered: root.feedback = "" }
  // Coalesces a burst of directory-change events (e.g. a rewrite lands as an
  // unlink+create, or a live share rewriting live.json up to once a second
  // plus at every state change) into one refresh instead of one --status per
  // event.
  Timer {
    id: watchDebounceTimer
    objectName: "watchDebounceTimer"
    interval: 300
    repeat: false
    onTriggered: root.refresh()
  }
  // Mirrors Omarchy's own FileView pattern: watch the live.json directory
  // (not the file) since FileView cannot watch a path before it exists, and
  // keep the polling Timer above as a fallback since a directory watch can
  // stop delivering events after flag changes land in quick succession.
  FileView {
    id: runtimeWatch
    objectName: "runtimeWatch"
    watchChanges: true
    printErrors: false
    // A live share rewrites live.json up to once a second, and at once on a
    // state change (each atomic rename can even surface as more than one
    // event); restarting the timer on every event could suppress the
    // refresh for as long as writes keep coming. Start it only on the first event
    // of a burst so it still fires ~300ms later regardless of how many
    // more events follow. While sharing or while the panel is open, ignore
    // events entirely: the 2s/1s poll already covers those states, so
    // reacting here would only add extra --status spawns.
    onFileChanged: {
      if (root.sessionOn || root.opened) return
      if (!watchDebounceTimer.running) watchDebounceTimer.start()
    }
  }
  Command {
    id: statusCommand
    objectName: "statusCommand"
    timeoutMs: 3500
    onCompleted: function(code, exitStatus, output) {
      root.armRuntimeWatch()
      root.receiveStatus(output, code, exitStatus, statusCommand.stderrTail)
    }
    onFailed: function(reason) {
      root.armRuntimeWatch()
      if (root.pollEpoch === root.statusEpoch)
        root.statusFailed(root.withCause("Could not check OmaBeam. " + reason + " Check that OmaBeam is installed.", statusCommand.stderrTail))
      else Qt.callLater(root.refresh)
    }
  }
  Command {
    id: stopCommand
    objectName: "stopCommand"
    // Native --stop waits up to 10s for the share to exit (its teardown IPC
    // has a 6s budget), then its own recovery of an extended display gets
    // another 6s budget: about 16s at worst, under this bound.
    timeoutMs: 25000
    onCompleted: function(code, exitStatus, output) {
      if (code !== 0 || exitStatus !== 0) {
        root.stopPending = false
        root.showFeedback(root.withCause("Could not stop sharing. Try again.", stopCommand.stderrTail), true)
      }
      root.refresh()
    }
    // A timeout is not proof the share is still running: extended-display
    // cleanup can legitimately outlast even this bound. Keep stopPending and
    // let the status read this refresh triggers decide the real outcome.
    onFailed: root.refresh()
  }
  Command {
    id: qrCommand
    objectName: "qrCommand"
    maxBytes: 8192
    onCompleted: function(code, exitStatus, output) {
      var rows = code === 0 && exitStatus === 0 ? Session.readQr(output, root.qrUrl) : []
      qrCommand.output = ""
      if (!root.qrVisible || !content.canShare || root.qrUrl !== root.session.url) return
      root.qrRows = rows
      root.qrError = rows.length ? "" : root.withCause("Could not create the QR code. Copy the link instead.", qrCommand.stderrTail)
    }
    onFailed: {
      qrCommand.output = ""
      if (root.qrVisible) root.qrError = root.withCause("Could not create the QR code. Copy the link instead.", qrCommand.stderrTail)
    }
  }
  Command {
    id: copyCommand
    objectName: "copyCommand"
    maxBytes: 256
    onCompleted: function(code, exitStatus, output) {
      if (!root.sessionOn || root.session.url !== root.copiedUrl) return
      root.showFeedback(code === 0 && exitStatus === 0 ? "Share link copied." : root.withCause("Could not copy the link. Check your clipboard.", copyCommand.stderrTail), code !== 0 || exitStatus !== 0)
    }
    onFailed: {
      if (root.sessionOn && root.session.url === root.copiedUrl)
        root.showFeedback(root.withCause("Could not copy the link. Check that wl-copy is available.", copyCommand.stderrTail), true)
    }
  }
  Command {
    id: sendCommand
    objectName: "sendCommand"
    collectOutput: false
    timeoutMs: 0
    onStarted: { sendCommand.deadline.stop(); root.close() }
    onCompleted: function(code, exitStatus, output) {
      if (code !== 0 || exitStatus !== 0) {
        root.showFeedback(root.withCause("Could not send the link. Copy it instead, or try again.", sendCommand.stderrTail), true)
        root.open()
      }
      root.refresh()
    }
    onFailed: root.showFeedback(root.withCause("Could not open the send window. Copy the link instead.", sendCommand.stderrTail), true)
  }
  Command {
    id: pickerCommand
    objectName: "pickerCommand"
    collectOutput: false
    timeoutMs: 0
    // The picker stays running until dismissed; do not buffer its stdout.
    onStarted: { pickerCommand.deadline.stop(); root.close() }
    onCompleted: function(code, exitStatus, output) {
      if (code !== 0 || exitStatus !== 0) {
        root.showFeedback(root.withCause("OmaBeam could not open the picker. Try again.", pickerCommand.stderrTail), true)
        root.open()
      }
      root.refresh()
    }
    onFailed: root.showFeedback(root.withCause("Could not open OmaBeam. Check that it is installed.", pickerCommand.stderrTail), true)
  }

  Shell.KeyboardPanel {
    id: panel
    anchorItem: root.anchorItem
    owner: root.barIdentity
    bar: root.bar
    open: root.opened
    // Keep the panel attached to the icon the user clicked.
    centerOnBar: false
    focusTarget: keyCatcher
    padding: Style.space(18)
    contentWidth: panel.fittedContentWidth(Style.space(400))
    contentHeight: panel.fittedContentHeight(content.implicitHeight)

    Shell.PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      onCloseRequested: root.close()
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onMoveRequested: function(dx, dy) { content.moveCursor(dx, dy) }
      onActivateRequested: content.activateCursor()
      onTextKey: function(text) {
        if (text === "c") root.copyUrl()
        if (text === "o") root.openViewer()
        if (text === "n") root.sendNearby()
        if (text === "q") root.toggleQr()
        if (text === "r") root.refresh()
      }
      ShareContent {
        id: content
        anchors.fill: parent
        session: root.session
        ready: root.ready
        statusError: root.statusError
        feedback: root.feedback
        feedbackError: root.feedbackError
        copying: copyCommand.pending
        stopping: root.stopPending
        launching: pickerCommand.pending
        sendingLink: sendCommand.pending
        qrVisible: root.qrVisible
        qrRows: root.qrRows
        qrError: root.qrError
        qrBusy: qrCommand.pending || qrCommand.running
        onQrRequested: root.toggleQr()
        foreground: Color.popups.text
        background: Color.popups.background
        accent: Color.accent
        urgent: root.bar ? root.bar.urgent : Color.urgent
        fontFamily: root.bar ? root.bar.fontFamily : Style.font.family
        bodySize: Style.font.body
        captionSize: Style.font.caption
        unit: Style.space(1)
        cornerRadius: Style.cornerRadius
        onCopyRequested: root.copyUrl()
        onViewerRequested: root.openViewer()
        onSendRequested: root.sendNearby()
        onStopRequested: root.stopLive()
        onPickerRequested: root.launchPicker()
        onRefreshRequested: root.refresh()
      }
    }
  }
}
