import QtQuick
import QtQuick.Layouts
import Quickshell
import Quickshell.Io
import Quickshell.Wayland
import Quickshell.Hyprland
import qs.Commons
import qs.Commons as Commons
import qs.Ui

// Host requests from sandboxes, shown the moment they arrive, without opening
// the panel. Cards appear at the top of the focused screen.
//
// Safety: the card never takes the keyboard when it appears, so typing in
// another window cannot answer it. Answer with the mouse, or press the
// keyboard shortcut (IPC `harnessix.paddock.approvals keys`) to answer with
// the keyboard; keyboard mode starts on "No".
//
// The wording follows the permission prompts of agent CLIs: No, Yes once,
// Yes for this session. No answer is styled as the primary one.
Item {
  id: root
  property var shell: null

  Commons.Translations {
    id: strings
    directory: Qt.resolvedUrl("i18n")
  }

  property var requests: []
  // Answered requests stay for a moment with their result.
  property var settled: ({})
  property bool keyboardMode: false
  property int choice: 0
  property real now: Date.now() / 1000

  readonly property var visibleRequests: requests.slice(0, 3)
  readonly property string findPaddock: "pd() { for p in \"$(command -v paddock)\" \"$HOME/.local/bin/paddock\" \"$HOME/.cargo/bin/paddock\"; do if [ -n \"$p\" ] && [ -x \"$p\" ]; then \"$p\" \"$@\"; return; fi; done; return 127; }"
  readonly property string launcher: findPaddock + "; pd \"$@\""

  // Logging, as the Omarchy shell does it: silent normally, console.warn on
  // a real fallback (once per message, the polls repeat every few seconds),
  // and console.log only with PADDOCK_DEBUG=true in the shell's environment.
  // Read the output with: journalctl -t omarchy-shell
  readonly property bool debug: Quickshell.env("PADDOCK_DEBUG") === "true"
  property string lastWarning: ""
  function warnOnce(msg) {
    if (msg === lastWarning) return
    lastWarning = msg
    console.warn("paddock: " + msg)
  }
  function debugLog(msg) { if (debug) console.log("paddock: " + msg) }

  function plain(val, maxLen) {
    var s = String(val === undefined || val === null ? "" : val)
    return s.length > maxLen ? s.slice(0, maxLen) : s
  }
  function baseName(path) {
    var parts = String(path || "").split("/").filter(function(x) { return x.length > 0 })
    return parts.length ? parts[parts.length - 1] : ""
  }
  function answersFor(r) {
    var list = ["deny", "once"]
    if (r && r.sessionAllowed) list.push("session")
    return list
  }
  // The first http(s) link in a request's detail.
  function linkIn(r) {
    var m = String(r.detail || "").match(/https?:\/\/[^\s"'<>]+/)
    return m ? m[0] : ""
  }
  // A link that starts a login: it carries an OAuth callback or code flow.
  // The harness then waits for the callback on a localhost port, which
  // paddock forwards for ten minutes after the page opens.
  function isLogin(r) {
    if (r.capability !== "open-url") return false
    var u = linkIn(r).toLowerCase()
    return /[?&](redirect_uri|redirect_url|response_type|code_challenge)=/.test(u) || /\/(oauth2?|authorize|login|device)(\/|\?|$)/.test(u)
  }
  function linkHost(r) {
    var m = linkIn(r).match(/^https?:\/\/([^\/?#:]+)/)
    return m ? m[1] : ""
  }
  function question(r) {
    if (isLogin(r)) return strings.t("popup.ask.login").arg(r.harness).arg(linkHost(r))
    var key = "popup.ask." + r.capability
    var text = strings.t(key)
    if (text === key) text = strings.t("popup.ask.other")
    return text.arg(r.harness).arg(r.projectName)
  }
  function glyph(c) {
    return ({ "screenshot": "󰹑", "exec": "󰆍", "fetch": "󰉋", "clipboard-read": "󰅇",
              "clipboard-write": "󰅌", "open-url": "󰌹", "notify": "󰂞" })[c] || "󰀦"
  }

  function answer(r, decision) {
    if (root.settled[r.id]) return
    var s = Object.assign({}, root.settled)
    s[r.id] = { state: "sending", decision: decision }
    root.settled = s
    answerProc.pending = { id: r.id, decision: decision }
    answerProc.command = ["sh", "-c", root.launcher, "paddock", "approvals", "answer", r.id, decision]
    answerProc.running = true
  }
  function finish(id, ok, decision) {
    var s = Object.assign({}, root.settled)
    s[id] = { state: ok ? (decision === "deny" ? "denied" : "allowed") : "gone", decision: decision }
    root.settled = s
    root.keyboardMode = false
    clearTimer.ids = clearTimer.ids.concat([id])
    clearTimer.restart()
  }

  function refresh() {
    if (!listProc.running) { listProc.buffer = ""; listProc.running = true }
  }

  // Tells paddock that this card is on screen, so it can skip the plain
  // desktop notification for the same request.
  function heartbeat() {
    Quickshell.execDetached(["sh", "-c", "d=\"${XDG_RUNTIME_DIR:-/tmp}/paddock\"; mkdir -p \"$d\" && date +%s > \"$d/approval-ui\""])
  }

  Timer {
    interval: root.requests.length > 0 ? 1000 : 2500
    running: true
    repeat: true
    triggeredOnStart: true
    onTriggered: { root.now = Date.now() / 1000; root.refresh() }
  }
  Timer {
    interval: 5000
    running: true
    repeat: true
    triggeredOnStart: true
    onTriggered: root.heartbeat()
  }
  // Windows of app sandboxes get paddock's border, so you can tell a
  // protected app at a glance. paddock sets it per window at run time.
  Timer {
    interval: 3000
    running: true
    repeat: true
    triggeredOnStart: true
    onTriggered: if (!markProc.running) markProc.running = true
  }
  Process {
    id: markProc
    command: ["sh", "-c", root.launcher, "paddock", "windows", "--mark"]
  }

  Timer {
    id: clearTimer
    property var ids: []
    interval: 1600
    onTriggered: {
      var keep = []
      for (var i = 0; i < root.requests.length; i++) if (ids.indexOf(root.requests[i].id) < 0) keep.push(root.requests[i])
      var s = Object.assign({}, root.settled)
      for (var j = 0; j < ids.length; j++) delete s[ids[j]]
      root.settled = s
      root.requests = keep
      ids = []
    }
  }

  component JsonProc: Process {
    id: proc
    property string buffer: ""
    stdout: SplitParser {
      onRead: function(line) { if (proc.buffer.length < 262144) proc.buffer += line }
    }
  }

  JsonProc {
    id: listProc
    command: ["sh", "-c", root.launcher, "paddock", "approvals", "--json"]
    onExited: function(exitCode) {
      if (exitCode !== 0) { root.debugLog("approvals --json exited with " + exitCode); return }
      try {
        var p = JSON.parse(buffer)
        if (!p || p.api_version !== "paddock.approvals/v1" || !Array.isArray(p.pending)) return
        var fresh = []
        for (var i = 0; i < p.pending.length && fresh.length < 16; i++) {
          var r = p.pending[i] || {}
          var id = String(r.id || "")
          if (!/^[A-Za-z0-9-]{1,96}$/.test(id)) continue
          fresh.push({
            id: id,
            harness: root.plain(r.harness, 24),
            projectName: root.plain(root.baseName(r.project), 48),
            capability: root.plain(r.capability, 24),
            summary: root.plain(r.summary, 300),
            detail: root.plain(r.detail, 8000),
            reason: root.plain(r.reason, 1000),
            created: Number(r.created || 0),
            expires: Number(r.expires || 0),
            sessionAllowed: r.session_allowed === true
          })
        }
        // Keep cards that are showing their result until they leave.
        for (var j = 0; j < root.requests.length; j++) {
          var old = root.requests[j]
          if (root.settled[old.id] && !fresh.some(function(f) { return f.id === old.id })) fresh.push(old)
        }
        root.requests = fresh
        if (fresh.length === 0) root.keyboardMode = false
        root.lastWarning = ""
      } catch (e) {
        root.warnOnce("cannot read paddock approvals --json, keeping the old list: " + e)
      }
    }
  }

  Process {
    id: answerProc
    property var pending: null
    onExited: function(exitCode) {
      if (pending) root.finish(pending.id, exitCode === 0, pending.decision)
      pending = null
      root.refresh()
    }
  }

  ShellIpc {
    target: "harnessix.paddock.approvals"
    // Bind this to a key: it gives the keyboard to the waiting card.
    function keys(): void {
      if (root.requests.length === 0) return
      root.choice = 0
      root.keyboardMode = true
    }
    function release(): void { root.keyboardMode = false }
  }

  Variants {
    model: Quickshell.screens

    PanelWindow {
      id: surface
      required property var modelData
      screen: modelData
      readonly property bool isTarget: {
        var m = Hyprland.focusedMonitor
        return m ? m.name === modelData.name : modelData === Quickshell.screens[0]
      }
      readonly property bool showing: isTarget && root.visibleRequests.length > 0
      visible: showing
      color: "transparent"

      WlrLayershell.namespace: "paddock-approvals"
      WlrLayershell.layer: WlrLayer.Overlay
      // Never take the keyboard on arrival; only in keyboard mode.
      WlrLayershell.keyboardFocus: root.keyboardMode && showing ? WlrKeyboardFocus.Exclusive : WlrKeyboardFocus.None
      exclusionMode: ExclusionMode.Ignore

      anchors { top: true }
      margins.top: Style.space(52)
      implicitWidth: Style.space(480)
      implicitHeight: stack.implicitHeight + Style.space(16)

      Item {
        anchors.fill: parent
        focus: root.keyboardMode
        Keys.onPressed: function(event) {
          if (!root.keyboardMode || root.requests.length === 0) return
          var r = root.requests[0]
          var n = root.answersFor(r).length
          if (event.key === Qt.Key_Left) root.choice = Math.max(0, root.choice - 1)
          else if (event.key === Qt.Key_Right || event.key === Qt.Key_Tab) root.choice = Math.min(n - 1, root.choice + 1)
          else if (event.key === Qt.Key_Return || event.key === Qt.Key_Enter) root.answer(r, root.answersFor(r)[root.choice])
          else if (event.key === Qt.Key_Escape) root.keyboardMode = false
          event.accepted = true
        }
      }

      Column {
        id: stack
        anchors.top: parent.top
        anchors.horizontalCenter: parent.horizontalCenter
        width: parent.width - Style.space(16)
        spacing: Style.space(8)

        Repeater {
          model: root.visibleRequests
          delegate: Rectangle {
            id: card
            required property var modelData
            required property int index
            readonly property var result: root.settled[modelData.id] || null
            readonly property real secondsLeft: modelData.expires > 0 ? Math.max(0, modelData.expires - root.now) : 0
            readonly property real total: modelData.expires > modelData.created ? modelData.expires - modelData.created : 120
            readonly property color tone: result
              ? (result.state === "allowed" ? Commons.Color.accent : (result.state === "denied" ? Commons.Color.urgent : Qt.alpha(Commons.Color.foreground, 0.5)))
              : "#e0a33a"

            width: parent.width
            implicitHeight: body.implicitHeight + Style.space(28)
            radius: Style.cornerRadius * 1.5
            color: Commons.Color.background
            border.width: 1
            border.color: Qt.alpha(card.tone, 0.55)
            opacity: 0
            scale: 0.97
            Component.onCompleted: { opacity = 1; scale = 1 }
            Behavior on opacity { NumberAnimation { duration: Style.duration(180); easing.type: Easing.OutCubic } }
            Behavior on scale { NumberAnimation { duration: Style.duration(180); easing.type: Easing.Bezier; easing.bezierCurve: Style.easeOut } }

            ColumnLayout {
              id: body
              anchors.left: parent.left
              anchors.right: parent.right
              anchors.top: parent.top
              anchors.margins: Style.space(14)
              spacing: Style.space(8)

              // Status chip and position.
              RowLayout {
                Layout.fillWidth: true
                spacing: Style.space(8)
                Rectangle {
                  radius: height / 2
                  color: Qt.alpha(card.tone, 0.14)
                  border.width: 1
                  border.color: Qt.alpha(card.tone, 0.4)
                  implicitHeight: chip.implicitHeight + Style.space(6)
                  implicitWidth: chip.implicitWidth + Style.space(16)
                  Text {
                    id: chip
                    anchors.centerIn: parent
                    textFormat: Text.PlainText
                    color: card.tone
                    font.family: Style.font.family
                    font.pixelSize: Style.font.caption
                    font.bold: true
                    text: (root.isLogin(card.modelData) ? "󰌆" : root.glyph(card.modelData.capability)) + "  " + (card.result
                      ? strings.t("popup.state." + card.result.state)
                      : (root.isLogin(card.modelData) ? strings.t("popup.state.login") : strings.t("popup.state.pending")))
                  }
                }
                Item { Layout.fillWidth: true }
                Text {
                  textFormat: Text.PlainText
                  visible: !card.result
                  color: card.secondsLeft < 20 ? Commons.Color.urgent : Qt.alpha(Commons.Color.foreground, 0.6)
                  font.family: Style.font.family
                  font.pixelSize: Style.font.caption
                  text: (root.requests.length > 1 ? strings.t("popup.position").arg(card.index + 1).arg(root.requests.length) + "  ·  " : "")
                    + strings.tn("time.left", Math.round(card.secondsLeft)).arg(Commons.I18n.formatNumber(Math.round(card.secondsLeft)))
                }
              }

              // The question, then exactly what will happen.
              Text {
                Layout.fillWidth: true
                textFormat: Text.PlainText
                wrapMode: Text.Wrap
                color: Commons.Color.foreground
                font.family: Style.font.family
                font.pixelSize: Style.font.body
                font.bold: true
                text: root.question(card.modelData)
              }
              Rectangle {
                Layout.fillWidth: true
                radius: Style.cornerRadius
                color: Qt.alpha(Commons.Color.foreground, 0.06)
                border.width: 1
                border.color: Qt.alpha(Commons.Color.foreground, 0.10)
                implicitHeight: Math.min(detail.implicitHeight, Style.space(220)) + Style.space(14)
                clip: true
                Flickable {
                  anchors.fill: parent
                  anchors.margins: Style.space(7)
                  contentHeight: detail.implicitHeight
                  interactive: detail.implicitHeight > height
                  Text {
                    id: detail
                    width: parent.width
                    textFormat: Text.PlainText
                    wrapMode: Text.WrapAnywhere
                    color: Commons.Color.foreground
                    font.family: "monospace"
                    font.pixelSize: Style.font.caption
                    text: card.modelData.detail
                  }
                }
              }
              Text {
                Layout.fillWidth: true
                visible: card.modelData.reason !== ""
                textFormat: Text.PlainText
                wrapMode: Text.Wrap
                color: Qt.alpha(Commons.Color.foreground, 0.62)
                font.family: Style.font.family
                font.pixelSize: Style.font.caption
                font.italic: true
                text: strings.t("approve.reason").arg(card.modelData.reason)
              }

              // Answers: equal weight, "No" first.
              RowLayout {
                visible: !card.result
                spacing: Style.space(6)
                Repeater {
                  model: root.answersFor(card.modelData)
                  delegate: Button {
                    required property var modelData
                    required property int index
                    text: strings.t("popup.answer." + modelData)
                    bordered: true
                    fontFamily: Style.font.family
                    hasCursor: root.keyboardMode && card.index === 0 && root.choice === index
                    onClicked: root.answer(card.modelData, modelData)
                  }
                }
                Item { Layout.fillWidth: true }
                Text {
                  visible: card.index === 0 && !root.keyboardMode
                  textFormat: Text.PlainText
                  color: Qt.alpha(Commons.Color.foreground, 0.45)
                  font.family: Style.font.family
                  font.pixelSize: Style.font.caption
                  text: strings.t("popup.keysHint")
                }
              }
            }

            // Time left, as a thin bar along the bottom edge.
            Rectangle {
              visible: !card.result && card.modelData.expires > 0
              anchors.left: parent.left
              anchors.bottom: parent.bottom
              anchors.margins: 1
              height: Style.space(3)
              radius: height / 2
              width: (parent.width - 2) * Math.min(1, card.secondsLeft / card.total)
              color: card.secondsLeft < 20 ? Commons.Color.urgent : card.tone
              Behavior on width { NumberAnimation { duration: 900 } }
            }
          }
        }
      }
    }
  }
}
