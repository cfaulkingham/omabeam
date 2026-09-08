import QtQuick
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
  property string copiedUrl: ""

  readonly property var barIdentity: hostWidget || root
  readonly property bool sessionOn: session.state === "live"
  readonly property bool sessionProblem: statusError !== "" || session.state === "ended"
  readonly property string sessionSummary: statusError ? "OmaBeam · status unavailable"
    : !ready ? "OmaBeam · checking status"
    : sessionOn ? "Sharing " + session.title + (session.viewers === null ? ""
      : " · " + session.viewers + (session.viewers === 1 ? " viewer" : " viewers"))
    : session.state === "ended" ? "Share ended · " + (session.error || session.title)
    : "OmaBeam · choose a window, screen, or area"
  readonly property string omabeamBin: decodeURIComponent(String(Qt.resolvedUrl("omabeam")).replace(/^file:\/\//, ""))

  function open() {
    content.resetCursor()
    root.refresh()
    root.controller.show()
    Qt.callLater(function() { if (root.opened) root.setCenterHoverRevealSuppressed(true) })
  }
  function close() {
    root.setCenterHoverRevealSuppressed(false)
    root.controller.hide()
  }
  function toggle() { root.opened ? root.close() : root.open() }
  function setCenterHoverRevealSuppressed(value) {
    if (root.bar && "centerHoverRevealSuppressed" in root.bar) root.bar.centerHoverRevealSuppressed = value
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
  function refresh() {
    if (statusCommand.pending || stopCommand.pending) return
    pollEpoch = statusEpoch
    statusCommand.start([omabeamBin, "--status"])
  }
  function statusFailed(reason) {
    ready = true
    statusError = reason
    if (stopPending) {
      stopPending = false
      showFeedback("Could not confirm the share stopped. Check again before leaving.", true)
    }
  }
  function receiveStatus(raw, code, exitStatus) {
    if (pollEpoch !== statusEpoch) { Qt.callLater(root.refresh); return }
    if (code === 127) {
      statusFailed("OmaBeam needs its native app. Run ./install.sh --backend-only in the plugin folder, then check status again.")
      return
    }
    var next
    try { next = Session.read(raw, code, exitStatus) }
    catch (error) { statusFailed(error.message); return }
    if (next.url !== session.url || next.state !== session.state) {
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
    copiedUrl = session.url
    copyCommand.start(["wl-copy", "--", copiedUrl])
  }
  function openViewer() {
    if (!content.canShare) return
    if (!Qt.openUrlExternally(session.url)) showFeedback("Could not open your browser. Copy the link instead.", true)
  }
  function sendNearby() {
    if (!content.canShare || sendCommand.pending) return
    sendCommand.start([omabeamBin, "--send-link", session.url])
  }
  function stopLive() {
    if (!sessionOn || stopPending) return
    // Ignore any status request started before the stop command.
    statusEpoch += 1
    stopPending = true
    feedback = ""
    stopCommand.start([omabeamBin, "--stop"])
  }
  function launchPicker() {
    if (!ready || sessionOn || statusError || pickerCommand.pending) return
    pickerCommand.start([omabeamBin])
  }

  Timer {
    interval: root.opened ? 1000 : root.sessionOn ? 2000 : 5000
    running: true
    repeat: true
    onTriggered: root.refresh()
  }
  Timer { id: feedbackTimer; interval: root.feedbackError ? 7000 : 3000; onTriggered: root.feedback = "" }
  Command {
    id: statusCommand
    objectName: "statusCommand"
    timeoutMs: 3500
    onCompleted: function(code, exitStatus, output) { root.receiveStatus(output, code, exitStatus) }
    onFailed: function(reason) {
      if (root.pollEpoch === root.statusEpoch)
        root.statusFailed("Could not check OmaBeam. " + reason + " Check that OmaBeam is installed.")
      else Qt.callLater(root.refresh)
    }
  }
  Command {
    id: stopCommand
    objectName: "stopCommand"
    onCompleted: function(code, exitStatus, output) {
      if (code !== 0 || exitStatus !== 0) {
        root.stopPending = false
        root.showFeedback("Could not stop sharing. Try again.", true)
      }
      root.refresh()
    }
    onFailed: function(reason) {
      root.stopPending = false
      root.showFeedback("Could not stop sharing. " + reason, true)
      root.refresh()
    }
  }
  Command {
    id: copyCommand
    objectName: "copyCommand"
    onCompleted: function(code, exitStatus, output) {
      if (!root.sessionOn || root.session.url !== root.copiedUrl) return
      root.showFeedback(code === 0 && exitStatus === 0 ? "Share link copied." : "Could not copy the link. Check your clipboard.", code !== 0 || exitStatus !== 0)
    }
    onFailed: {
      if (root.sessionOn && root.session.url === root.copiedUrl)
        root.showFeedback("Could not copy the link. Check that wl-copy is available.", true)
    }
  }
  Command {
    id: sendCommand
    objectName: "sendCommand"
    onStarted: { sendCommand.deadline.stop(); root.close() }
    onCompleted: function(code, exitStatus, output) {
      if (code !== 0 || exitStatus !== 0) {
        root.showFeedback("Could not send the link. Copy it instead, or try again.", true)
        root.open()
      }
      root.refresh()
    }
    onFailed: root.showFeedback("Could not open the send window. Copy the link instead.", true)
  }
  Command {
    id: pickerCommand
    objectName: "pickerCommand"
    // The picker stays running until dismissed; only its startup is timed.
    onStarted: { pickerCommand.deadline.stop(); root.close() }
    onCompleted: function(code, exitStatus, output) {
      if (code !== 0 || exitStatus !== 0) {
        root.showFeedback("OmaBeam could not open the picker. Try again.", true)
        root.open()
      }
      root.refresh()
    }
    onFailed: root.showFeedback("Could not open OmaBeam. Check that it is installed.", true)
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
