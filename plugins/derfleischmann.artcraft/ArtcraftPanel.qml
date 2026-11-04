pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Layouts
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Commons as Commons
import qs.Ui

// ArtCraft popup. Reads `artcraftctl status --json` (cached unless a refresh
// is requested), lists pending updates and starts the verified upgrade in a
// floating terminal. Every upgrade inside artcraftctl re-checks SHA-256 and
// VirusTotal before a single byte is extracted.
Panel {
  id: root
  moduleName: "derfleischmann.artcraft"
  ipcTarget: "derfleischmann.artcraft"
  manageIpc: false

  property var anchorItem: null
  property var hostWidget: null
  readonly property var barIdentity: hostWidget || root

  readonly property color contentForeground: bar ? bar.foreground : Color.foreground
  readonly property color contentAccent: bar ? (bar.accent || Color.accent) : Color.accent
  readonly property color contentMuted: Color.muted
  readonly property color contentBorder: Color.popups.border
  readonly property string contentFontFamily: bar ? bar.fontFamily : Style.font.family

  readonly property string homeDir: Quickshell.env("HOME")
  readonly property string ctlPath: homeDir + "/.local/bin/artcraftctl"
  readonly property string hubPath: homeDir + "/.local/bin/artcraft-hub"

  // ---- state ------------------------------------------------------------

  property var status: ({})
  property bool loading: false
  property string error: ""
  property double loadedAt: 0
  property bool upgrading: false
  property string upgradeLog: ""
  property bool actionRunning: false

  readonly property var apps: status.apps || []
  readonly property var updates: apps.filter(function(app) { return app.update_available === true })
  readonly property var missing: apps.filter(function(app) { return app.not_installed === true && !!app.latest_version })
  readonly property var pinned: apps.filter(function(app) { return app.pinned === true })
  readonly property var skipped: apps.filter(function(app) { return app.skipped === true })
  readonly property int updateCount: Number(status.update_count) || 0
  readonly property int installedCount: Number(status.installed_count) || 0
  readonly property bool vtMissing: loadedAt > 0 && status.vt_configured === false

  readonly property string summary: {
    if (root.error !== "") return root.error
    if (root.loading && root.loadedAt === 0) return "Checking releases …"
    if (root.loadedAt === 0) return "Not checked yet"
    if (root.loading) return "Checking releases …"
    if (root.updateCount > 0) return root.installedCount + " installed · " + root.updateCount + " update(s) available"
    if (root.installedCount > 0) return root.installedCount + " installed · all up to date"
    return "No app installed yet"
  }

  readonly property string checkedLabel: {
    if (!root.loadedAt || !root.status.checked_at) return ""
    return "Checked: " + Qt.formatDateTime(new Date(Number(root.status.checked_at) * 1000), "dd.MM. HH:mm")
  }

  // ---- artcraftctl ------------------------------------------------------

  function refresh(force) {
    if (statusProc.running) return
    root.loading = true
    root.error = ""
    var subcommand = force ? "check" : "status"
    var ctl = root.ctlPath
    statusProc.command = [
      "/bin/sh", "-c",
      "A=\"$HOME/.local/bin/artcraftctl\"; if [ ! -x \"$A\" ]; then A=\"" + ctl + "\"; fi; exec \"$A\" " + subcommand + " --json"
    ]
    statusProc.running = true
    statusGuard.restart()
  }

  function applyStatus(text) {
    try {
      var data = JSON.parse(text)
      if (!data || Number(data.schema) !== 2 || !Array.isArray(data.apps))
        throw new Error("unknown format")
      root.status = data
      root.error = ""
      root.loadedAt = Date.now()
    } catch (e) {
      root.error = "artcraftctl returned no valid response"
    }
    root.loading = false
    statusGuard.stop()
  }

  function runInTerminal(command) {
    if (!root.bar) return
    root.bar.run("omarchy-launch-floating-terminal-with-presentation " + command)
    root.close()
  }

  function upgradeAll() {
    if (root.vtMissing || root.upgrading) return
    root.upgrading = true
    root.upgradeLog = ""
    upgradeProc.command = ["/bin/sh", "-c", "exec \"" + root.ctlPath + "\" upgrade --all --yes"]
    upgradeProc.running = true
  }

  function upgradeOne(id) {
    if (root.vtMissing || root.upgrading) return
    root.upgrading = true
    root.upgradeLog = ""
    upgradeProc.command = ["/bin/sh", "-c", "exec \"" + root.ctlPath + "\" upgrade " + id + " --yes"]
    upgradeProc.running = true
  }

  function openApp(id) {
    if (!root.bar) return
    root.bar.run("\"" + root.homeDir + "/.local/bin/" + id + "\"")
  }

  function openHub() {
    if (!root.bar) return
    root.bar.run("\"" + root.hubPath + "\"")
    root.close()
  }

  function appendUpgradeLog(text) {
    if (text && text.trim() !== "") root.upgradeLog += text + "\n"
  }

  function runAction(command) {
    if (root.actionRunning) return
    root.actionRunning = true
    actionProc.command = ["/bin/sh", "-c", "exec \"" + root.ctlPath + "\" " + command]
    actionProc.running = true
  }

  // ---- popup plumbing ---------------------------------------------------

  function open() {
    root.refresh(false)
    root.controller.show()
  }

  function close() {
    root.controller.hide()
  }

  function toggle() {
    if (root.opened) root.close()
    else root.open()
  }

  function switchPanel(direction) {
    if (root.bar && typeof root.bar.switchPanelFrom === "function")
      return root.bar.switchPanelFrom(root.barIdentity, direction)
    return false
  }

  // ---- UI ---------------------------------------------------------------

  KeyboardPanel {
    id: panel
    anchorItem: root.anchorItem
    owner: root.barIdentity
    bar: root.bar
    open: root.opened
    centerOnBar: false
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(460))
    contentHeight: panel.fittedContentHeight(contentColumn.implicitHeight)

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      onCloseRequested: root.close()
      onActivateRequested: {
        if (root.updateCount > 0) root.upgradeAll()
        else root.refresh(true)
      }
      onTabRequested: function(direction) { root.switchPanel(direction) }
      onTextKey: function(t) {
        if (t === "r" || t === "R") root.refresh(true)
        else if (t === "o" || t === "O") root.openHub()
      }

      Column {
        id: contentColumn
        anchors.fill: parent
        spacing: Style.space(9)

        // ---- header
        RowLayout {
          width: parent.width
          spacing: Style.space(8)

          Text {
            text: "\uf1fc"
            color: root.updateCount > 0 ? root.contentAccent : root.contentForeground
            font.family: root.contentFontFamily
            font.pixelSize: Style.font.subtitle
            Layout.alignment: Qt.AlignVCenter
          }

          ColumnLayout {
            Layout.fillWidth: true
            spacing: 0

            Text {
              text: "ArtCraft Hub"
              color: root.contentForeground
              font.family: root.contentFontFamily
              font.pixelSize: Style.font.subtitle
              font.weight: Font.DemiBold
              elide: Text.ElideRight
              Layout.fillWidth: true
            }

            Text {
              text: root.summary
              color: root.error !== "" ? root.contentAccent : root.contentMuted
              font.family: root.contentFontFamily
              font.pixelSize: Style.font.bodySmall
              elide: Text.ElideRight
              Layout.fillWidth: true
            }
          }

          Button {
            visible: root.bar !== null
            text: "Hub"
            tooltipText: "Open ArtCraft Hub"
            fontSize: Style.font.bodySmall
            foreground: root.contentForeground
            fontFamily: root.contentFontFamily
            bordered: true
            onClicked: root.openHub()
          }
        }

        Rectangle {
          width: parent.width
          height: 1
          color: root.contentBorder
        }

        // ---- VirusTotal warning
        Rectangle {
          visible: root.vtMissing
          width: parent.width
          height: vtColumn.implicitHeight + Style.space(14)
          radius: Style.space(6)
          color: Qt.rgba(0.94, 0.71, 0.31, 0.12)
          border.width: 1
          border.color: Qt.rgba(0.94, 0.71, 0.31, 0.45)

          Column {
            id: vtColumn
            anchors.centerIn: parent
            width: parent.width - Style.space(16)
            spacing: Style.space(2)

            Text {
              text: "VirusTotal key missing"
              color: Commons.Color.urgent
              font.family: root.contentFontFamily
              font.pixelSize: Style.font.bodySmall
              font.weight: Font.DemiBold
            }

            Text {
              width: parent.width
              text: "Installs and updates are locked until a key is set in the Hub."
              color: root.contentMuted
              font.family: root.contentFontFamily
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.WordWrap
            }
          }
        }

        // ---- pending updates
        Flickable {
          id: updateScroll
          visible: root.updateCount > 0
          width: parent.width
          height: Math.min(updateList.implicitHeight, Style.space(300))
          contentWidth: width
          contentHeight: updateList.implicitHeight
          clip: true
          boundsBehavior: Flickable.StopAtBounds
          interactive: updateList.implicitHeight > height

          Column {
            id: updateList
            width: updateScroll.width
            spacing: Style.space(4)

            Repeater {
              model: root.updates

              delegate: RowLayout {
                id: updateRow
                required property var modelData
                width: updateList.width
                spacing: Style.space(8)

                Image {
                  source: "file://" + root.homeDir + "/.local/share/icons/hicolor/48x48/apps/ai.storyteller." + modelData.id + ".png"
                  width: Style.space(20)
                  height: width
                  fillMode: Image.PreserveAspectFit
                  Layout.alignment: Qt.AlignVCenter
                }

                Text {
                  text: modelData.name
                  color: root.contentForeground
                  font.family: root.contentFontFamily
                  font.pixelSize: Style.font.body
                  elide: Text.ElideRight
                  Layout.fillWidth: true
                  MouseArea {
                    anchors.fill: parent
                    cursorShape: Qt.PointingHandCursor
                    onClicked: root.openApp(modelData.id)
                  }
                }

                Text {
                  text: (modelData.installed_version || "–") + " → " + (modelData.latest_version || "?")
                  color: root.contentMuted
                  font.family: root.contentFontFamily
                  font.pixelSize: Style.font.bodySmall
                }

                Button {
                  text: "Update"
                  fontSize: Style.font.bodySmall
                  foreground: root.contentForeground
                  fontFamily: root.contentFontFamily
                  bordered: true
                  enabled: !root.vtMissing && !root.upgrading
                  onClicked: root.upgradeOne(modelData.id)
                }

                Button {
                  visible: !!modelData.latest_tag
                  text: "Skip"
                  fontSize: Style.font.bodySmall
                  foreground: root.contentForeground
                  fontFamily: root.contentFontFamily
                  bordered: true
                  enabled: !root.actionRunning
                  onClicked: root.runAction("skip " + modelData.id + " " + modelData.latest_tag)
                }
              }
            }
          }
        }

        Text {
          visible: root.loadedAt > 0 && !root.loading && root.error === "" && root.updateCount === 0
          width: parent.width
          text: root.installedCount > 0
            ? "All installed apps are up to date."
            : "Open the Hub to install the apps."
          color: root.contentMuted
          font.family: root.contentFontFamily
          font.pixelSize: Style.font.bodySmall
          wrapMode: Text.WordWrap
        }

        // ---- pinned
        Column {
          visible: root.pinned.length > 0
          width: parent.width
          spacing: Style.space(2)

          Text {
            text: "PINNED"
            color: root.contentMuted
            font.family: root.contentFontFamily
            font.pixelSize: Style.font.bodySmall
            font.letterSpacing: 1
          }

          Repeater {
            model: root.pinned

            delegate: RowLayout {
              required property var modelData
              width: parent.width
              spacing: Style.space(8)

              Text {
                text: modelData.name
                color: root.contentForeground
                font.family: root.contentFontFamily
                font.pixelSize: Style.font.body
                elide: Text.ElideRight
                Layout.fillWidth: true
              }

              Text {
                text: modelData.pinned_tag || ""
                color: root.contentMuted
                font.family: root.contentFontFamily
                font.pixelSize: Style.font.bodySmall
              }

              Button {
                text: "Unpin"
                fontSize: Style.font.bodySmall
                foreground: root.contentForeground
                fontFamily: root.contentFontFamily
                bordered: true
                enabled: !root.actionRunning
                onClicked: root.runAction("unpin " + modelData.id)
              }
            }
          }
        }

        // ---- skipped
        Column {
          visible: root.skipped.length > 0
          width: parent.width
          spacing: Style.space(2)

          Text {
            text: "SKIPPED"
            color: root.contentMuted
            font.family: root.contentFontFamily
            font.pixelSize: Style.font.bodySmall
            font.letterSpacing: 1
          }

          Repeater {
            model: root.skipped

            delegate: RowLayout {
              required property var modelData
              width: parent.width
              spacing: Style.space(8)

              Text {
                text: modelData.name
                color: root.contentForeground
                font.family: root.contentFontFamily
                font.pixelSize: Style.font.body
                elide: Text.ElideRight
                Layout.fillWidth: true
              }

              Text {
                text: "latest skipped"
                color: root.contentMuted
                font.family: root.contentFontFamily
                font.pixelSize: Style.font.bodySmall
              }

              Button {
                text: "Unskip"
                fontSize: Style.font.bodySmall
                foreground: root.contentForeground
                fontFamily: root.contentFontFamily
                bordered: true
                enabled: !root.actionRunning
                onClicked: root.runAction("unskip " + modelData.id)
              }
            }
          }
        }
        Column {
          visible: root.missing.length > 0
          width: parent.width
          spacing: Style.space(2)

          Text {
            text: "NOT INSTALLED"
            color: root.contentMuted
            font.family: root.contentFontFamily
            font.pixelSize: Style.font.bodySmall
            font.letterSpacing: 1
          }

          Text {
            width: parent.width
            text: root.missing.map(function(app) { return app.name }).join(" · ")
            color: root.contentForeground
            font.family: root.contentFontFamily
            font.pixelSize: Style.font.bodySmall
            wrapMode: Text.WordWrap
          }
        }

        // ---- upgrade progress
        Column {
          visible: root.upgrading
          width: parent.width
          spacing: Style.space(6)

          RowLayout {
            width: parent.width
            spacing: Style.space(8)

            Text {
              text: "⟳"
              color: root.contentAccent
              font.family: root.contentFontFamily
              font.pixelSize: Style.font.subtitle
            }

            Text {
              text: "Upgrading…"
              color: root.contentForeground
              font.family: root.contentFontFamily
              font.pixelSize: Style.font.body
            }
          }

          Flickable {
            width: parent.width
            height: Style.space(120)
            contentWidth: width
            contentHeight: upgradeLogText.implicitHeight
            clip: true
            boundsBehavior: Flickable.StopAtBounds

            Text {
              id: upgradeLogText
              width: parent.width
              text: root.upgradeLog
              color: root.contentMuted
              font.family: root.contentFontFamily
              font.pixelSize: Style.font.bodySmall
              wrapMode: Text.Wrap
            }
          }
        }

        Rectangle {
          width: parent.width
          height: 1
          color: root.contentBorder
        }

        // ---- footer
        RowLayout {
          width: parent.width
          spacing: Style.space(6)

          Text {
            text: "Shortcuts: r refresh · o open hub · Enter update all · click an app to open"
            color: root.contentMuted
            font.family: root.contentFontFamily
            font.pixelSize: Style.font.bodySmall
            elide: Text.ElideRight
            Layout.fillWidth: true
          }

          Button {
            visible: root.updateCount > 0
            text: "Update all"
            tooltipText: "SHA-256 and VirusTotal will be re-checked"
            fontSize: Style.font.bodySmall
            foreground: root.contentForeground
            fontFamily: root.contentFontFamily
            bordered: true
            enabled: !root.vtMissing && !root.upgrading && !root.loading && !root.upgrading
            onClicked: root.upgradeAll()
          }

          Item { Layout.fillWidth: true }

          Text {
            visible: root.checkedLabel !== ""
            text: root.checkedLabel
            color: root.contentMuted
            font.family: root.contentFontFamily
            font.pixelSize: Style.font.bodySmall
          }

          Button {
            text: "Check now"
            tooltipText: "Check for new releases now (r)"
            fontSize: Style.font.bodySmall
            foreground: root.contentForeground
            fontFamily: root.contentFontFamily
            bordered: true
            enabled: !root.loading
            onClicked: root.refresh(true)
          }
        }
      }
    }
  }

  Process {
    id: upgradeProc
    running: false
    command: ["/bin/sh", "-c", "exec \"$HOME/.local/bin/artcraftctl\" upgrade --all --yes"]

    stdout: StdioCollector {
      id: upgradeOut
      waitForEnd: true
      onStreamFinished: root.appendUpgradeLog(text)
    }

    stderr: StdioCollector {
      id: upgradeErr
      waitForEnd: true
      onStreamFinished: root.appendUpgradeLog(text)
    }

    onExited: function(exitCode) {
      root.upgrading = false
      root.refresh(true)
    }
  }

  Process {
    id: statusProc
    running: false
    command: ["/bin/sh", "-c", "exec \"$HOME/.local/bin/artcraftctl\" status --json"]

    stdout: StdioCollector {
      id: statusOut
      waitForEnd: true
      onStreamFinished: root.applyStatus(text)
    }

    onExited: function(exitCode) {
      if (exitCode !== 0 && root.loading) {
        root.loading = false
        root.error = "artcraftctl is not reachable (exit " + exitCode + ")"
        statusGuard.stop()
      }
    }
  }

  Timer {
    id: statusGuard
    interval: 120000
    repeat: false
    onTriggered: {
      if (!statusProc.running) return
      statusProc.running = false
      root.loading = false
      root.error = "Timed out while checking releases"
    }
  }

  Process {
    id: actionProc
    running: false
    command: ["/bin/sh", "-c", "exec \"$HOME/.local/bin/artcraftctl\" status --json"]

    onExited: function(exitCode) {
      root.actionRunning = false
      root.refresh(true)
    }
  }

  // Keep the bar badge honest even when the popup is never opened.
  Timer {
    interval: 30 * 60 * 1000
    running: true
    repeat: true
    triggeredOnStart: true
    onTriggered: root.refresh(false)
  }
}
