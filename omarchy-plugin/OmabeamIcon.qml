pragma ComponentBehavior: Bound
import QtQuick

// 2×2 rounded tiles; the trailing tile is faded, matching the panel mark.
Item {
  id: root
  property real iconSize: 16
  property color color: "#a5c9ad"

  width: iconSize
  height: iconSize
  implicitWidth: iconSize
  implicitHeight: iconSize

  readonly property real gap: iconSize * 3 / 17
  readonly property real tile: iconSize * 7 / 17
  readonly property real tileRadius: Math.max(1, iconSize / 17)

  Grid {
    anchors.centerIn: parent
    columns: 2
    spacing: root.gap
    Repeater {
      model: 4
      Rectangle {
        required property int index
        width: root.tile
        height: width
        radius: root.tileRadius
        color: root.color
        opacity: index === 3 ? 0.35 : 1
      }
    }
  }
}
