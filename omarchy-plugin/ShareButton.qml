import QtQuick

Rectangle {
  id: root
  property string text: ""
  property color foreground: "white"
  property color fill: "transparent"
  property string fontFamily: ""
  property real fontSize: 14
  property real unit: 1
  property real cornerRadius: 6 * unit
  property bool primary: false
  property bool hasCursor: false
  signal clicked()
  signal hovered()

  implicitHeight: Math.max(42 * unit, label.implicitHeight + 22 * unit)
  implicitWidth: label.implicitWidth + 32 * unit
  radius: cornerRadius
  color: primary ? foreground : Qt.rgba(foreground.r, foreground.g, foreground.b,
    mouse.containsMouse || hasCursor ? 0.12 : 0.04)
  border.width: hasCursor ? 2 * unit : unit
  border.color: Qt.rgba(foreground.r, foreground.g, foreground.b, hasCursor ? 0.9 : 0.18)
  opacity: enabled ? 1 : 0.45
  Accessible.role: Accessible.Button
  Accessible.name: text
  Accessible.focusable: true
  Accessible.focused: hasCursor
  Accessible.onPressAction: if (enabled) clicked()

  Text {
    id: label
    anchors.centerIn: parent
    width: Math.max(0, parent.width - 24 * root.unit)
    text: root.text
    textFormat: Text.PlainText
    horizontalAlignment: Text.AlignHCenter
    elide: Text.ElideRight
    color: root.primary ? root.fill : root.foreground
    font.family: root.fontFamily
    font.pixelSize: root.fontSize
    font.weight: Font.DemiBold
  }
  // An inset outline keeps keyboard selection visible on the filled CTA too.
  Rectangle {
    anchors.fill: parent
    anchors.margins: 3 * root.unit
    visible: root.primary && root.hasCursor
    color: "transparent"
    border.width: root.unit
    border.color: root.fill
    radius: Math.max(0, root.cornerRadius - 2 * root.unit)
  }
  MouseArea {
    id: mouse
    anchors.fill: parent
    enabled: root.enabled
    hoverEnabled: true
    cursorShape: Qt.PointingHandCursor
    onEntered: root.hovered()
    onClicked: root.clicked()
  }
}
