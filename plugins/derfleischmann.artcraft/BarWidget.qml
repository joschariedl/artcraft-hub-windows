import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

// ArtCraft bar widget: a paintbrush that turns urgent while updates are
// waiting, plus the popup with the details. The panel owns the artcraftctl
// calls; this widget only forwards the bar contract (open/close/opened) and
// exposes the update count so the bar can style the button.
BarWidget {
  id: root
  moduleName: "derfleischmann.artcraft"

  readonly property bool opened: panelLoader.item ? panelLoader.item.opened === true : false
  readonly property int updateCount: panelLoader.item ? (panelLoader.item.updateCount || 0) : 0
  readonly property bool vtMissing: panelLoader.item ? panelLoader.item.vtMissing === true : false

  function open() { if (panelLoader.item) panelLoader.item.open() }
  function close() { if (panelLoader.item) panelLoader.item.close() }
  function togglePanel() { if (panelLoader.item) panelLoader.item.toggle() }

  // Forwarded so this widget can stand in for the panel as the bar's popout
  // identity (same contract as the clock and feedflut widgets).
  readonly property bool popoutSwitchClosing: panelLoader.item ? panelLoader.item.popoutSwitchClosing === true : false
  function closeForPopoutSwitch() { if (panelLoader.item) panelLoader.item.closeForPopoutSwitch() }

  property var panelItem: null

  function injectPanel() {
    var target = panelLoader.item
    if (!target) return
    panelItem = target
    if ("bar" in target) target.bar = root.bar
    if ("settings" in target) target.settings = root.settings
    if ("anchorItem" in target) target.anchorItem = button
    if ("hostWidget" in target) target.hostWidget = root
  }

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  onBarChanged: injectPanel()
  onSettingsChanged: injectPanel()

  Loader {
    id: panelLoader
    active: true
    source: Qt.resolvedUrl("ArtcraftPanel.qml")
    visible: false
    onLoaded: {
      root.injectPanel()
      Qt.callLater(root.injectPanel)
    }
  }

  IpcHandler {
    target: "derfleischmann.artcraft"

    function refresh(): void { if (panelLoader.item) panelLoader.item.refresh(true) }
    function open(): void { root.open() }
    function close(): void { root.close() }
    function show(): void { root.open() }
    function hide(): void { root.close() }
    function toggle(): void { root.togglePanel() }
  }

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: "\uf1fc"
    active: root.updateCount > 0
    tooltipText: {
      if (root.updateCount > 0)
        return "ArtCraft: " + root.updateCount + " update(s) available"
      if (root.vtMissing)
        return "ArtCraft: VirusTotal key missing"
      return "ArtCraft Hub"
    }
    onPressed: function(b) {
      if (b === Qt.LeftButton) root.togglePanel()
      else if (b === Qt.MiddleButton) root.open()
    }
  }

  // Small dot in the corner, so the indicator reads at a glance even when the
  // urgent colour is close to the bar's foreground.
  Rectangle {
    visible: root.updateCount > 0
    anchors.right: parent.right
    anchors.top: parent.top
    width: Style.space(5)
    height: width
    radius: width / 2
    color: root.bar ? root.bar.urgent : Color.urgent
  }
}
