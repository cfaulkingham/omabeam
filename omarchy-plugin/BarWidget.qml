import QtQuick
import qs.Commons
import qs.Ui as Shell
import "Session.js" as Session

Shell.BarWidget {
  id: root
  moduleName: "io.github.cfaulkingham.omabeam"

  property bool sessionOn: false
  property bool sessionProblem: false
  property string sessionSummary: "OmaBeam · checking status"

  readonly property bool opened: panelLoader.item
    ? panelLoader.item.opened === true
    : false
  readonly property bool popoutSwitchClosing: panelLoader.item
    ? panelLoader.item.popoutSwitchClosing === true
    : false

  function open() {
    if (panelLoader.item) panelLoader.item.open()
  }

  function close() {
    if (panelLoader.item) panelLoader.item.close()
  }

  function toggle() {
    if (panelLoader.item) panelLoader.item.toggle()
  }

  function closeForPopoutSwitch() {
    if (panelLoader.item) panelLoader.item.closeForPopoutSwitch()
  }

  function injectPanel() {
    if (!panelLoader.item) return
    panelLoader.item.bar = root.bar
    panelLoader.item.settings = root.settings
    panelLoader.item.anchorItem = button
    panelLoader.item.hostWidget = root
    panelLoader.item.syncHost()
  }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  onBarChanged: injectPanel()
  onSettingsChanged: injectPanel()

  Loader {
    id: panelLoader
    active: true
    source: Qt.resolvedUrl("Panel.qml")
    visible: false
    onLoaded: {
      root.injectPanel()
      Qt.callLater(root.injectPanel)
    }
  }

  Shell.BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    active: root.sessionOn
    dimmed: !root.sessionOn
    tooltipText: Session.plain(root.sessionSummary)
    iconComponent: Component {
      Item {
        OmabeamIcon {
          anchors.centerIn: parent
          iconSize: Style.space(14)
          color: button.foreground
        }
      }
    }
    onPressed: function(buttonCode) {
      if (buttonCode === Qt.RightButton || buttonCode === Qt.LeftButton) {
        root.toggle()
      }
    }
    Rectangle {
      visible: root.sessionOn || root.sessionProblem
      anchors.right: parent.right
      anchors.bottom: parent.bottom
      anchors.margins: Style.space(3)
      width: Style.space(5)
      height: width
      radius: width / 2
      color: root.sessionOn ? (root.bar ? root.bar.urgent : Color.urgent) : "transparent"
      border.width: Style.space(1)
      border.color: root.bar ? root.bar.urgent : Color.urgent
    }
  }
}
