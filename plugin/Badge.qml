import QtQuick
import Quickshell
import Quickshell.Wayland
import Quickshell.Hyprland
import qs.Commons
import qs.Commons as Commons

// The corner badge of a protected window: a small pill on the top edge, at
// the right corner, for example "paddock · dev · egress · 2h". It shows for
// a few seconds after a protected window gets focus, and while the pointer
// is on it. On Omarchy the window under the pointer gets focus, so a hover
// over a protected window shows it too. A click opens that sandbox in the
// paddock panel.
//
// The pill is a layer-shell overlay. Its input region is the pill only, so
// it never takes clicks from the windows under it. It follows the window
// geometry through Hyprland IPC events. Without Hyprland (the offline render
// harness), the geometry and focus come from `paddock windows --json`.
Item {
  id: root

  // Windows from `paddock windows --json`, cleaned by Service.qml.
  property var windows: []
  property bool lightTheme: false

  // Seconds that the badge stays after a focus change.
  readonly property int holdMs: 4000

  // Hyprland's window list changes without a notify for each field: bump a
  // counter after a refresh, so the bindings read lastIpcObject again.
  property int hyprRevision: 0
  readonly property bool hyprland: (Quickshell.env("HYPRLAND_INSTANCE_SIGNATURE") || "") !== ""

  property string shownAddress: ""
  property bool hovered: false
  property string lastFocus: ""

  function norm(a) { return String(a || "").replace(/^0x/, "").toLowerCase() }

  function levelColor(level) {
    if (level === "proxy") return root.lightTheme ? "#4f7a55" : "#9fb8a0"
    return root.lightTheme ? "#a8701a" : "#e0a33a"
  }

  // The live Hyprland data of a window: { at, size, focused }, or null.
  function live(address) {
    var _ = root.hyprRevision
    if (!root.hyprland || !Hyprland.toplevels) return null
    var list = Hyprland.toplevels.values
    for (var i = 0; i < list.length; i++) {
      var ipc = list[i].lastIpcObject
      if (ipc && root.norm(ipc.address) === root.norm(address))
        return { at: ipc.at || [0, 0], size: ipc.size || [0, 0], focused: Number(ipc.focusHistoryID) === 0,
                 hidden: ipc.hidden === true || ipc.mapped === false }
    }
    return null
  }

  function find(address) {
    for (var i = 0; i < root.windows.length; i++)
      if (root.norm(root.windows[i].address) === root.norm(address)) return root.windows[i]
    return null
  }

  // The focused protected window, or "".
  readonly property string focusedAddress: {
    var _ = root.hyprRevision
    for (var i = 0; i < root.windows.length; i++) {
      var w = root.windows[i]
      var l = live(w.address)
      if (l ? l.focused : w.focused) return w.address
    }
    return ""
  }

  onFocusedAddressChanged: {
    if (focusedAddress === lastFocus) return
    lastFocus = focusedAddress
    if (focusedAddress !== "") { shownAddress = focusedAddress; holdTimer.restart() }
    else if (!hovered) shownAddress = ""
  }

  Timer {
    id: holdTimer
    interval: root.holdMs
    onTriggered: if (!root.hovered) root.shownAddress = ""
  }

  readonly property var shown: shownAddress === "" ? null : find(shownAddress)
  // The last badge, so the pill keeps its text while it fades out.
  property var lastShown: null
  onShownChanged: if (shown !== null) lastShown = shown
  // A window that closed or lost its run takes the badge with it.
  onWindowsChanged: if (shownAddress !== "" && find(shownAddress) === null) shownAddress = ""

  Connections {
    target: root.hyprland ? Hyprland : null
    function onRawEvent(event) {
      switch (event.name) {
      case "openwindow": case "closewindow": case "movewindowv2": case "activewindowv2":
      case "workspacev2": case "focusedmonv2": case "changefloatingmode": case "fullscreen":
        hyprRefresh.restart()
      }
    }
  }
  Timer {
    id: hyprRefresh
    interval: 60
    onTriggered: { Hyprland.refreshToplevels(); hyprSettle.restart() }
  }
  Timer {
    id: hyprSettle
    interval: 150
    onTriggered: root.hyprRevision++
  }

  Variants {
    model: Quickshell.screens

    PanelWindow {
      id: surface
      required property var modelData
      screen: modelData

      // The window geometry in global coordinates.
      readonly property var geo: {
        var w = root.shown
        if (!w) return null
        var l = root.live(w.address)
        if (l && l.hidden) return null
        var at = l ? l.at : w.at
        var size = l ? l.size : w.size
        return { x: Number(at[0]), y: Number(at[1]), w: Number(size[0]), h: Number(size[1]) }
      }
      // The badge belongs to the screen that holds the window's center.
      // The last geometry, so the pill fades out where it was.
      property var lastGeo: null
      onGeoChanged: if (geo) lastGeo = geo
      readonly property bool here: {
        if (!geo) return false
        var cx = geo.x + geo.w / 2, cy = geo.y + geo.h / 2
        return cx >= modelData.x && cx < modelData.x + modelData.width
          && cy >= modelData.y && cy < modelData.y + modelData.height
      }

      visible: here || pill.opacity > 0
      color: "transparent"
      WlrLayershell.namespace: "paddock-badge"
      WlrLayershell.layer: WlrLayer.Top
      WlrLayershell.keyboardFocus: WlrKeyboardFocus.None
      exclusionMode: ExclusionMode.Ignore
      anchors { top: true; bottom: true; left: true; right: true }
      // Only the pill takes the pointer.
      mask: Region { item: surface.here ? pill : null }

      Rectangle {
        id: pill
        readonly property var w: root.lastShown
        readonly property var g: surface.lastGeo
        readonly property color tone: w ? root.levelColor(w.level) : "transparent"
        // "paddock" in the level color, then the details.
        readonly property string details: w ? String(w.badge || "").replace(/^paddock · /, "") : ""

        // On the top edge, at the right corner, inside the screen.
        x: g ? Math.max(Style.space(4), Math.min(surface.width - width - Style.space(4),
             g.x - surface.modelData.x + g.w - width - Style.space(14))) : 0
        y: g ? Math.max(Style.space(2), Math.min(surface.height - height - Style.space(2),
             g.y - surface.modelData.y - Math.round(height / 2))) : 0
        width: row.implicitWidth + Style.space(18)
        height: Math.round(Style.font.bodySmall * 2)
        radius: height / 2
        color: Qt.alpha(Commons.Color.background, 0.96)
        border.width: 1
        border.color: Qt.alpha(tone, 0.85)
        opacity: surface.here ? 1 : 0
        transform: Translate { y: surface.here ? 0 : -Style.space(4); Behavior on y { NumberAnimation { duration: Style.duration(160); easing.type: Easing.OutCubic } } }
        Behavior on opacity { NumberAnimation { duration: Style.duration(160); easing.type: Easing.OutCubic } }

        Row {
          id: row
          anchors.centerIn: parent
          spacing: Style.space(6)

          // The privacy dot: the level color, always the same place.
          Rectangle {
            anchors.verticalCenter: parent.verticalCenter
            width: Style.space(6)
            height: width
            radius: width / 2
            color: pill.tone
          }
          Text {
            anchors.verticalCenter: parent.verticalCenter
            text: "paddock"
            color: pill.tone
            font.family: Style.font.family
            font.pixelSize: Style.font.bodySmall
            font.weight: Font.DemiBold
          }
          Text {
            anchors.verticalCenter: parent.verticalCenter
            text: "· " + pill.details
            color: Qt.alpha(Commons.Color.foreground, 0.82)
            font.family: Style.font.family
            font.pixelSize: Style.font.bodySmall
          }
        }

        MouseArea {
          anchors.fill: parent
          hoverEnabled: true
          cursorShape: Qt.PointingHandCursor
          onContainsMouseChanged: {
            root.hovered = containsMouse
            if (!containsMouse) holdTimer.restart()
          }
          onClicked: {
            var id = pill.w ? String(pill.w.sandbox) : ""
            if (/^[A-Za-z0-9._-]{1,96}$/.test(id))
              Quickshell.execDetached(["omarchy-shell", "harnessix.paddock", "show", id])
            root.shownAddress = ""
            root.hovered = false
          }
        }
      }
    }
  }
}
