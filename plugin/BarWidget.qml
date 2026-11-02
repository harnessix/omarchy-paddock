import QtQuick
import Quickshell
import qs.Commons
import qs.Commons as Commons
import qs.Ui

BarWidget {
  id: root
  moduleName: "harnessix.paddock"

  Commons.Translations {
    id: strings
    directory: Qt.resolvedUrl("i18n")
  }

  readonly property bool opened: panelLoader.item ? panelLoader.item.opened === true : false
  readonly property int runningCount: panelLoader.item ? panelLoader.item.runningCount : 0
  readonly property string pluginState: panelLoader.item ? panelLoader.item.pluginState : "ok"
  readonly property int pendingCount: panelLoader.item ? panelLoader.item.pendingCount : 0
  readonly property var endedNotice: panelLoader.item ? panelLoader.item.endedNotice : null
  readonly property bool popoutSwitchClosing: panelLoader.item ? panelLoader.item.popoutSwitchClosing === true : false

  function open(payloadJson) { if (panelLoader.item) panelLoader.item.open(payloadJson) }
  function close() { if (panelLoader.item) panelLoader.item.close() }
  function togglePanel() { if (panelLoader.item) panelLoader.item.toggle() }
  function closeForPopoutSwitch() { if (panelLoader.item) panelLoader.item.closeForPopoutSwitch() }

  function injectPanel() {
    var target = panelLoader.item
    if (!target) return
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
    source: Qt.resolvedUrl("Panel.qml")
    visible: false
    onLoaded: {
      root.injectPanel()
      Qt.callLater(root.injectPanel)
    }
  }

  ShellIpc {
    target: "harnessix.paddock"
    function open(): void { root.open("") }
    function close(): void { root.close() }
    function toggle(): void { root.togglePanel() }
  }

  WidgetButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    // paddock's mark: a fence (Material Design "fence"); with a pending host
    // request, the fence with an alert mark ("gate-alert").
    readonly property string glyph: "󱞚"
    readonly property string alertGlyph: "󱟸"
    readonly property bool iconOnly: root.vertical
      || (root.runningCount === 0 && root.pendingCount === 0 && root.settings && root.settings.hideWhenIdle === "Icon only")
    // Running sandboxes, then requests that wait for an answer, then for a
    // short while the run that just ended.
    text: {
      if (root.pluginState === "missing") return glyph + " !"
      var parts = []
      if (root.runningCount > 0) parts.push(glyph + " " + Commons.I18n.formatNumber(root.runningCount))
      if (root.pendingCount > 0) parts.push(alertGlyph + " " + Commons.I18n.formatNumber(root.pendingCount))
      if (root.endedNotice !== null && !root.iconOnly)
        parts.push((root.endedNotice.status === "failed" ? "󰅙 " : "󰄬 ") + root.endedNotice.harness
          + " " + strings.t("ended." + root.endedNotice.status))
      if (parts.length) return parts.join("  ")
      return iconOnly ? glyph : glyph + " " + strings.t("bar.label")
    }
    labelVisible: !iconOnly
    tooltipText: root.pluginState === "missing"
      ? strings.t("state.missing")
      : root.pendingCount > 0
      ? strings.tn("approve.header", root.pendingCount).arg(Commons.I18n.formatNumber(root.pendingCount))
      : strings.tn("bar.running", root.runningCount).arg(Commons.I18n.formatNumber(root.runningCount))
    // The urgent color is for requests that need an answer, not for running sandboxes.
    active: root.pendingCount > 0 || (root.endedNotice !== null && root.endedNotice.status === "failed")
    onPressed: function(b) { root.togglePanel() }
  }
}
