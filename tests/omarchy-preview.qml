import QtQuick
import "../omarchy-plugin" as OmaBeam

Rectangle {
  id: root
  width: 400
  height: content.implicitHeight + 36
  color: content.background
  property alias content: content
  property int qrRequests: 0
  property int copies: 0
  property int stops: 0
  property int viewers: 0
  property int sends: 0
  property int launches: 0
  property int refreshes: 0
  OmaBeam.ShareContent {
    id: content
    objectName: "shareContent"
    anchors.fill: parent
    anchors.margins: 18
    ready: true
    fontFamily: previewFontFamily
    focus: true
    onCopyRequested: root.copies += 1
    onQrRequested: root.qrRequests += 1
    onStopRequested: root.stops += 1
    onViewerRequested: root.viewers += 1
    onSendRequested: root.sends += 1
    onPickerRequested: root.launches += 1
    onRefreshRequested: root.refreshes += 1
    Keys.onPressed: function(event) {
      if (event.key === Qt.Key_Down) moveCursor(0, 1)
      else if (event.key === Qt.Key_Up) moveCursor(0, -1)
      else if (event.key === Qt.Key_Return) activateCursor()
      else return
      event.accepted = true
    }
  }
}
