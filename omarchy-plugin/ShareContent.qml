pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Layouts
import "Session.js" as Session

// Pure QtQuick presentation: the shell owns commands, lifecycle, and focus.
Item {
  id: root
  property var session: Session.empty()
  property bool ready: false
  property string statusError: ""
  property string feedback: ""
  property bool feedbackError: false
  property bool copying: false
  property bool stopping: false
  property bool launching: false
  property bool sendingLink: false
  property bool qrVisible: false
  property bool qrBusy: false
  property var qrRows: []
  property string qrError: ""
  signal qrRequested()
  property color foreground: "#d6deeb"
  property color background: "#161b24"
  property color accent: "#a5c9ad"
  property color urgent: "#e49393"
  property string fontFamily: ""
  property real bodySize: 14
  property real captionSize: 12
  property real unit: 1
  property real cornerRadius: 6 * unit
  property string selectedAction: "primary"
  readonly property bool live: session.state === "live"
  readonly property bool ended: session.state === "ended"
  readonly property bool casting: session.destination === "cast"
  readonly property bool connecting: live && casting && session.connection !== "streaming"
  readonly property bool reconnecting: live && casting && session.connection === "reconnecting"
  readonly property bool uncertain: statusError !== ""
  readonly property bool canShare: live && !uncertain && session.url !== "" && !stopping
  readonly property color muted: Qt.rgba(foreground.r, foreground.g, foreground.b, 0.68)
  readonly property color outline: Qt.rgba(foreground.r, foreground.g, foreground.b, 0.12)
  readonly property string heading: uncertain ? "Status unavailable" : !ready ? "Checking your share…"
    : reconnecting ? "Reconnecting to " + session.receiver
    : connecting ? "Connecting to " + session.receiver
    : live && casting ? "Casting to " + session.receiver
    : live ? "You're sharing" : ended ? "Your share has ended" : "Ready when you are"
  signal copyRequested()
  signal viewerRequested()
  signal sendRequested()
  signal stopRequested()
  signal pickerRequested()
  signal refreshRequested()

  implicitHeight: Math.ceil(header.implicitHeight) + Math.ceil(details.implicitHeight)
    + Math.ceil(footer.implicitHeight) + 32 * unit

  function resetCursor() { selectedAction = live && casting && !uncertain ? "stop" : "primary"; scroller.contentY = 0 }
  function actions() {
    var all = [primary, qr, viewer, send, stop]
    return all.filter(function(button) { return button.visible && button.enabled })
  }
  function moveCursor(dx, dy) {
    var list = actions()
    if (!list.length) return
    var index = list.map(function(button) { return button.objectName }).indexOf(selectedAction)
    var step = dx || dy
    if (index < 0) index = step > 0 ? 0 : list.length - 1
    else index = (index + (step > 0 ? 1 : -1) + list.length) % list.length
    selectedAction = list[index].objectName
  }
  function activateCursor() {
    var list = actions()
    for (var i = 0; i < list.length; ++i)
      if (list[i].objectName === selectedAction) { list[i].clicked(); return }
  }
  onLiveChanged: resetCursor()
  onCastingChanged: resetCursor()
  onUncertainChanged: resetCursor()

  ColumnLayout {
    anchors.fill: parent
    spacing: 16 * root.unit

    RowLayout {
      id: header
      Layout.fillWidth: true
      spacing: 12 * root.unit
      Rectangle {
        Layout.preferredWidth: 38 * root.unit
        Layout.preferredHeight: 38 * root.unit
        radius: root.cornerRadius
        color: Qt.rgba(root.accent.r, root.accent.g, root.accent.b, 0.1)
        OmabeamIcon {
          anchors.centerIn: parent
          iconSize: 17 * root.unit
          color: root.accent
        }
      }
      ColumnLayout {
        Layout.fillWidth: true
        spacing: 3 * root.unit
        Text {
          text: "OmaBeam"
          textFormat: Text.PlainText
          color: root.foreground
          font.family: root.fontFamily
          font.pixelSize: root.bodySize
          font.weight: Font.DemiBold
        }
        Text {
          Layout.fillWidth: true
          text: root.heading
          textFormat: Text.PlainText
          elide: Text.ElideRight
          color: root.muted
          font.family: root.fontFamily
          font.pixelSize: root.captionSize
        }
      }
      Rectangle {
        visible: root.live || root.ended
        implicitWidth: badge.implicitWidth + 18 * root.unit
        implicitHeight: badge.implicitHeight + 10 * root.unit
        radius: height / 2
        color: Qt.rgba(root.urgent.r, root.urgent.g, root.urgent.b, 0.12)
        Text {
          id: badge
          anchors.centerIn: parent
          text: root.stopping ? "STOPPING" : root.uncertain ? "CHECK" : root.reconnecting ? "RECONNECTING" : root.connecting ? "CONNECTING" : root.live ? "● LIVE" : "ENDED"
          textFormat: Text.PlainText
          color: root.urgent
          font.family: root.fontFamily
          font.pixelSize: root.captionSize * 0.85
          font.weight: Font.Bold
          font.letterSpacing: root.unit
        }
      }
    }

    Flickable {
      id: scroller
      objectName: "detailsScroller"
      Layout.fillWidth: true
      Layout.fillHeight: true
      Layout.preferredHeight: details.implicitHeight
      Layout.minimumHeight: 40 * root.unit
      contentWidth: width
      contentHeight: details.implicitHeight
      flickableDirection: Flickable.VerticalFlick
      boundsBehavior: Flickable.StopAtBounds
      clip: true

      Column {
        id: details
        width: scroller.width
        spacing: 16 * root.unit

        Rectangle {
          width: parent.width
          height: sourceBody.implicitHeight + 36 * root.unit
          radius: root.cornerRadius
          color: Qt.rgba(root.foreground.r, root.foreground.g, root.foreground.b, 0.035)
          border.width: root.unit
          border.color: root.outline
          Column {
            id: sourceBody
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.top: parent.top
            anchors.margins: 18 * root.unit
            spacing: 12 * root.unit
            RowLayout {
              width: parent.width
              Text {
                Layout.fillWidth: true
                text: root.live ? "SHARING NOW" : root.ended ? "LAST SHARED" : "YOUR NEXT SHARE"
                textFormat: Text.PlainText
                color: root.muted
                font.family: root.fontFamily
                font.pixelSize: root.captionSize * 0.85
                font.weight: Font.DemiBold
                font.letterSpacing: 1.3 * root.unit
              }
              Text {
                visible: root.live || root.ended
                text: Session.sourceKind(root.session)
                textFormat: Text.PlainText
                color: root.muted
                font.family: root.fontFamily
                font.pixelSize: root.captionSize
              }
            }
            Text {
              objectName: "sourceTitle"
              width: parent.width
              text: root.live || root.ended ? root.session.title : "Choose what to share."
              textFormat: Text.PlainText
              maximumLineCount: 3
              wrapMode: Text.Wrap
              elide: Text.ElideRight
              color: root.foreground
              font.family: root.fontFamily
              font.pixelSize: root.bodySize * 1.35
              font.weight: Font.DemiBold
              lineHeight: 1.15
            }
            Text {
              width: parent.width
              text: root.live || root.ended ? (Session.quality(root.session) || "Stream details will appear here.")
                : "Pick a window, screen, or area. Preview it before going live."
              textFormat: Text.PlainText
              wrapMode: Text.Wrap
              color: root.muted
              font.family: root.fontFamily
              font.pixelSize: root.captionSize
              lineHeight: 1.3
            }
          }
        }

        RowLayout {
          visible: root.live
          width: parent.width
          spacing: 20 * root.unit
          Repeater {
            model: [
              { value: root.uncertain || root.session.viewers === null ? "—" : String(Math.floor(root.session.viewers)),
                label: root.casting ? "receiver connected" : root.session.viewers === 1 ? "viewer connected" : "viewers connected" },
              { value: root.uncertain ? "—" : Session.elapsed(root.session.uptime), label: "time sharing" }
            ]
            Column {
              id: metric
              required property var modelData
              Layout.fillWidth: true
              Layout.preferredWidth: 1
              spacing: 5 * root.unit
              Text {
                text: metric.modelData.value
                textFormat: Text.PlainText
                color: root.foreground
                font.family: root.fontFamily
                font.pixelSize: root.bodySize * 1.8
                font.weight: Font.Medium
              }
              Text {
                width: parent.width
                text: metric.modelData.label
                textFormat: Text.PlainText
                wrapMode: Text.Wrap
                color: root.muted
                font.family: root.fontFamily
                font.pixelSize: root.captionSize
              }
            }
          }
        }

        Column {
          visible: root.live && !root.uncertain
          width: parent.width
          spacing: 8 * root.unit
          Rectangle { width: parent.width; height: root.unit; color: root.outline }
          Text {
            width: parent.width
            text: root.casting ? root.session.receiver : root.session.url ? Session.linkHost(root.session.url) : "Share link unavailable"
            textFormat: Text.PlainText
            elide: Text.ElideMiddle
            color: root.foreground
            font.family: root.fontFamily
            font.pixelSize: root.captionSize
          }
          Text {
            width: parent.width
            text: root.casting ? (root.reconnecting ? "Trying to resume this screen for up to 15 seconds."
                : root.connecting ? "Waiting for the receiver to accept the screen."
              : "Your selected source is being sent directly to this receiver.")
              : root.session.url ? "Scan the QR code, copy the link, or send it nearby. Open it on a device on the same network—no viewer app needed."
              : "The share is running, but its link could not be read. You can still stop it below."
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            color: root.muted
            font.family: root.fontFamily
            font.pixelSize: root.captionSize
            lineHeight: 1.25
          }
        }

        Column {
          id: qrCard
          objectName: "qrCard"
          visible: root.qrVisible && root.canShare
          width: parent.width
          spacing: 10 * root.unit
          onVisibleChanged: if (visible) Qt.callLater(function() {
            scroller.contentY = Math.max(0, Math.min(qrCard.y, scroller.contentHeight - scroller.height))
          })
          Canvas {
            id: qrCanvas
            objectName: "qrCanvas"
            visible: root.qrRows.length > 0
            readonly property var rows: root.qrRows
            // Integer modules and a four-module white quiet zone, independent of theme.
            readonly property int moduleSize: Math.max(1, Math.floor(Math.min(parent.width, 240 * root.unit) / (rows.length + 8)))
            width: (rows.length + 8) * moduleSize
            height: width
            anchors.horizontalCenter: parent.horizontalCenter
            antialiasing: false
            Accessible.role: Accessible.Graphic
            Accessible.name: "Scan to open the share on another device"
            onRowsChanged: requestPaint()
            onModuleSizeChanged: requestPaint()
            onPaint: {
              var ctx = getContext("2d")
              ctx.fillStyle = "#ffffff"
              ctx.fillRect(0, 0, width, height)
              ctx.fillStyle = "#000000"
              for (var y = 0; y < rows.length; ++y)
                for (var x = 0; x < rows.length; ++x)
                  if (rows[y].charAt(x) === "1")
                    ctx.fillRect((x + 4) * moduleSize, (y + 4) * moduleSize, moduleSize, moduleSize)
            }
          }
          Text {
            width: parent.width
            text: root.qrError || (root.qrRows.length ? "Scan with your camera on the same network. Anyone with this code can view your share." : "Creating QR code…")
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            horizontalAlignment: Text.AlignHCenter
            color: root.qrError ? root.urgent : root.muted
            font.family: root.fontFamily
            font.pixelSize: root.captionSize
          }
        }

        Rectangle {
          visible: root.uncertain || root.ended
          width: parent.width
          height: issue.implicitHeight + 24 * root.unit
          radius: root.cornerRadius
          color: Qt.rgba(root.urgent.r, root.urgent.g, root.urgent.b, 0.08)
          Text {
            id: issue
            anchors.top: parent.top
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.margins: 12 * root.unit
            text: root.statusError || root.session.error || "This source is no longer being shared. Choose a source to start again."
            textFormat: Text.PlainText
            wrapMode: Text.Wrap
            color: root.foreground
            font.family: root.fontFamily
            font.pixelSize: root.captionSize
            lineHeight: 1.3
          }
        }
      }
      Rectangle {
        // A small scroll indicator makes clipped details discoverable.
        parent: scroller
        visible: scroller.contentHeight > scroller.height + root.unit
        anchors.right: parent.right
        width: 3 * root.unit
        height: Math.max(16 * root.unit, scroller.height * scroller.visibleArea.heightRatio)
        y: scroller.visibleArea.yPosition * scroller.height
        radius: width / 2
        color: root.muted
      }
    }

    Column {
      id: footer
      Layout.fillWidth: true
      spacing: 8 * root.unit

      Text {
        objectName: "actionFeedback"
        width: parent.width
        // Reserve one line so copying never shifts the buttons under the mouse.
        text: root.feedback || (root.live && !root.casting && root.session.viewers === 0 && !root.uncertain ? "Waiting for your first viewer" : " ")
        textFormat: Text.PlainText
        wrapMode: Text.Wrap
        color: root.feedbackError ? root.urgent : root.muted
        font.family: root.fontFamily
        font.pixelSize: root.captionSize
        bottomPadding: 3 * root.unit
        Accessible.role: Accessible.StaticText
        Accessible.name: text.trim()
      }

      ShareButton {
        id: primary
        visible: !root.casting || !root.live || root.uncertain
        objectName: "primary"
        width: parent.width
        unit: root.unit
        cornerRadius: root.cornerRadius
        foreground: root.accent
        fill: root.background
        fontFamily: root.fontFamily
        fontSize: root.bodySize
        primary: true
        hasCursor: root.selectedAction === objectName
        text: root.uncertain ? "Check status again" : root.copying ? "Copying…"
          : root.live ? "Copy share link" : root.launching ? "Picker is open" : "Choose a source  →"
        enabled: root.uncertain ? !root.stopping : root.live ? root.canShare && !root.copying : root.ready && !root.launching
        onHovered: root.selectedAction = objectName
        onClicked: root.uncertain ? root.refreshRequested() : root.live ? root.copyRequested() : root.pickerRequested()
      }
      ShareButton {
        id: qr
        objectName: "qr"
        width: parent.width
        visible: root.live && !root.casting
        unit: root.unit
        cornerRadius: root.cornerRadius
        foreground: root.foreground
        fontFamily: root.fontFamily
        fontSize: root.captionSize
        text: root.qrVisible ? "Hide QR code" : "Show QR code"
        enabled: root.canShare && (root.qrVisible || !root.qrBusy)
        hasCursor: root.selectedAction === objectName
        onHovered: root.selectedAction = objectName
        onClicked: root.qrRequested()
      }
      Row {
        width: parent.width
        visible: root.live
        spacing: 8 * root.unit
        ShareButton {
          id: viewer
          objectName: "viewer"
          visible: root.live && !root.casting
          width: (parent.width - parent.spacing * 2) / 3
          unit: root.unit
          cornerRadius: root.cornerRadius
          foreground: root.foreground
          fontFamily: root.fontFamily
          fontSize: root.captionSize
          text: "Open viewer  ↗"
          enabled: root.canShare
          hasCursor: root.selectedAction === objectName
          onHovered: root.selectedAction = objectName
          onClicked: root.viewerRequested()
        }
        ShareButton {
          id: send
          objectName: "send"
          visible: root.live && !root.casting
          width: (parent.width - parent.spacing * 2) / 3
          unit: root.unit
          cornerRadius: root.cornerRadius
          foreground: root.foreground
          fontFamily: root.fontFamily
          fontSize: root.captionSize
          text: root.sendingLink ? "Opening…" : "Send nearby"
          enabled: root.canShare && !root.sendingLink
          hasCursor: root.selectedAction === objectName
          onHovered: root.selectedAction = objectName
          onClicked: root.sendRequested()
        }
        ShareButton {
          id: stop
          objectName: "stop"
          visible: root.live
          width: root.casting ? parent.width : (parent.width - parent.spacing * 2) / 3
          unit: root.unit
          cornerRadius: root.cornerRadius
          foreground: root.urgent
          fontFamily: root.fontFamily
          fontSize: root.captionSize
          text: root.stopping ? "Stopping…" : root.casting ? "Stop casting" : "Stop sharing"
          enabled: !root.stopping
          hasCursor: root.selectedAction === objectName
          onHovered: root.selectedAction = objectName
          onClicked: root.stopRequested()
        }
      }
      Text {
        width: parent.width
        topPadding: 5 * root.unit
        text: "↑↓ actions    ↵ select    esc close"
        textFormat: Text.PlainText
        color: root.muted
        font.family: root.fontFamily
        font.pixelSize: root.captionSize * 0.85
        horizontalAlignment: Text.AlignHCenter
      }
    }
  }
}
