import QtQuick
import QtQuick.Layouts
import QtQuick.Controls
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Commons as Commons
import qs.Ui

// The paddock panel. One keyboard and mouse cursor, as in Omarchy's own panels:
//   list view     Up/Down pick a sandbox, Enter goes to its terminal,
//                 Left/Right reach the row actions, Delete stops (press twice),
//                 N starts a new sandbox.
//   new sandbox   pick a recent project, then a harness; Enter opens a terminal
//                 that runs it. Escape goes back.
// Every value from paddock is shown as plain text and passed to commands as a
// separate argument, never interpolated into a shell string.
Panel {
  id: root
  moduleName: "harnessix.paddock"
  ipcTarget: "harnessix.paddock"
  manageIpc: false

  property var anchorItem: null
  property var hostWidget: null
  readonly property var barIdentity: hostWidget || root

  Commons.Translations {
    id: strings
    directory: Qt.resolvedUrl("i18n")
  }

  // Data from the paddock command.
  property int runningCount: 0
  // Requests from sandboxes to do something on the host. The fields come from
  // `paddock approvals --json`. detail and reason are untrusted text.
  property var approvals: []
  readonly property int pendingCount: approvals.length
  // The last answered requests, newest first, from `paddock log --json`.
  property var recent: []
  property var sandboxes: []
  // Files that sandboxes saved outside the project, from `paddock files --json`.
  // One entry per file; paths and names are untrusted text.
  property var artifacts: []
  // Keys of files already seen, so only new ones raise a notice.
  property var seenArtifacts: ({})
  property bool artifactsLoaded: false
  property var recentProjects: []
  // paddock kv. The overview has counts and names only, never a value; a
  // value is read when the user opens one key, and shown as plain text.
  property var kvScopes: []          // rows of the kv view
  property int kvKeyTotal: 0
  property var kvTarget: null        // the scope open in the keys view
  property var kvKeys: []            // its keys, without values
  property var kvCollapsed: ({})     // key groups the user closed
  property var kvEntry: null         // the key open in the key view
  readonly property bool kvVisible: kvScopes.length > 0
  readonly property int kvPastureCount: kvScopes.filter(function(t) { return t.kind === "pasture" }).length
  readonly property var kvRows: buildKvRows(kvKeys, kvCollapsed)
  property var harnesses: []
  // "ok", "missing" (paddock is not on PATH), or "error"
  property string pluginState: "ok"
  property bool loaded: false

  // View and cursor.
  property string view: "list"          // list | project | harness | kv | keys | key
  property int selectedIndex: 0
  property int actionIndex: -1           // -1: the row itself; 0..n: a row action
  property bool cursorActive: false
  property string confirmId: ""
  property string chosenProject: ""
  property string message: ""

  readonly property color fg: bar ? bar.barForeground : Commons.Color.foreground
  readonly property color dim: Qt.alpha(fg, 0.62)
  readonly property color urgent: Commons.Color.urgent
  readonly property color hoverFill: Style.hoverFillFor(fg, Commons.Color.accent)
  readonly property color selectedFill: Style.selectedFillFor(fg, Commons.Color.accent)
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family
  readonly property int refreshIntervalMs: Math.max(2, Number(setting("refreshIntervalSec", 10))) * 1000

  // paddock is looked up on PATH, then in ~/.local/bin and ~/.cargo/bin.
  // $HOME is expanded by the shell at run time; no user path is stored here.
  readonly property string findPaddock: "pd() { for p in \"$(command -v paddock)\" \"$HOME/.local/bin/paddock\" \"$HOME/.cargo/bin/paddock\"; do if [ -n \"$p\" ] && [ -x \"$p\" ]; then \"$p\" \"$@\"; return; fi; done; return 127; }"
  readonly property string launcher: findPaddock + "; pd \"$@\""
  // Starts an app sandbox without a terminal: the app has its own windows.
  // Arguments: app name, project folder. paddock shows errors as a notification.
  readonly property string appLauncher: "for p in \"$(command -v paddock)\" \"$HOME/.local/bin/paddock\" \"$HOME/.cargo/bin/paddock\"; do [ -n \"$p\" ] && [ -x \"$p\" ] && break; p=; done; [ -n \"$p\" ] || exit 127; if command -v uwsm-app >/dev/null 2>&1; then exec setsid uwsm-app -- \"$p\" app run \"$1\" \"$2\"; else exec setsid \"$p\" app run \"$1\" \"$2\"; fi"

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
  // Opens a terminal. Arguments: directory, title, then the command to run in it.
  readonly property string terminal: "dir=\"$1\"; title=\"$2\"; shift 2; if command -v uwsm-app >/dev/null 2>&1; then exec setsid uwsm-app -- xdg-terminal-exec --dir=\"$dir\" --title=\"$title\" -e \"$@\"; else exec setsid xdg-terminal-exec --dir=\"$dir\" --title=\"$title\" -e \"$@\"; fi"
  // Keeps a terminal open after a command ends.
  readonly property string holdOpen: findPaddock + "; \"$@\"; echo; printf '%s' \"$PADDOCK_HOLD\"; read -r _"

  // Row actions of a sandbox, in cursor order.
  function actionsFor(s) {
    var a = ["folder", "copy"]
    if (s && s.windowPid > 0) a.push("display")
    if (s && s.canFreeze) a.push("freeze")
    a.push("stop")
    return a
  }

  function open(payloadJson) {
    if (!logProc.running) { logProc.lines = []; logProc.running = true }
    root.view = "list"
    root.selectedIndex = 0
    root.actionIndex = -1
    root.message = ""
    refresh()
    root.controller.show()
    // refresh() reads paddock kv only while the panel is open.
    refreshKv(true)
  }
  function close() { root.confirmId = ""; root.view = "list"; root.controller.hide() }
  function toggle() { if (root.opened) root.close(); else root.open("") }

  function plain(val, maxLen) {
    var s = String(val === undefined || val === null ? "" : val)
    return s.length > maxLen ? s.slice(0, maxLen) : s
  }

  function baseName(path) {
    var parts = String(path || "").split("/").filter(function(x) { return x.length > 0 })
    return parts.length ? parts[parts.length - 1] : String(path || "")
  }

  function shortPath(path) {
    var home = Quickshell.env("HOME") || ""
    var p = String(path || "")
    return home !== "" && p.indexOf(home + "/") === 0 ? "~" + p.slice(home.length) : p
  }

  function uptime(started) {
    var mins = Math.max(0, Math.floor((Date.now() / 1000 - Number(started || 0)) / 60))
    if (mins < 60) return strings.tn("time.minutes", mins).arg(Commons.I18n.formatNumber(mins))
    var hours = Math.floor(mins / 60)
    return strings.tn("time.hours", hours).arg(Commons.I18n.formatNumber(hours))
  }

  function say(text) {
    root.message = text
    messageTimer.restart()
  }

  function rowCount() {
    // The first row of the project step opens a folder dialog.
    if (view === "project") return recentProjects.length + 1
    if (view === "harness") return installedHarnesses().length
    if (view === "kv") return kvScopes.length
    if (view === "keys") return kvRows.length
    if (view === "key") return 0
    return approvals.length + sandboxes.length + artifacts.length + (kvVisible ? 1 : 0)
  }

  // In the list view, approvals come first, then sandboxes, in one cursor order.
  function isApprovalRow(i) { return view === "list" && i < approvals.length }
  function sandboxAt(i) { return sandboxes[i - approvals.length] }
  // File rows follow the sandboxes.
  function isArtifactRow(i) { return view === "list" && artifacts.length > 0 && i >= approvals.length + sandboxes.length && i < approvals.length + sandboxes.length + artifacts.length }
  // The shared data row comes last.
  function isKvRow(i) { return view === "list" && kvVisible && i === approvals.length + sandboxes.length + artifacts.length }
  function artifactAt(i) { return artifacts[i - approvals.length - sandboxes.length] }
  function dirName(path) {
    var p = String(path || "")
    var k = p.lastIndexOf("/")
    return k > 0 ? p.slice(0, k) : p
  }
  function recoverFile(f) {
    runPaddock(["recover", f.id, "--", f.path], function(code) {
      root.say(code === 0 ? strings.t("msg.recovered").arg(root.shortPath(f.target)) : strings.t("msg.recoverFailed"))
      root.actionIndex = -1
      root.refreshFiles()
    })
  }
  function revealFile(f) {
    Quickshell.execDetached(["xdg-open", root.dirName(f.home + "/" + f.path)])
    root.close()
  }
  function refreshFiles() {
    if (!filesProc.running) { filesProc.buffer = ""; filesProc.running = true }
  }
  // A desktop notification for files that appeared since the last check.
  // It never takes the keyboard. notify-send gets only a count: project
  // names and paths come from sandboxes, and the arguments of a process are
  // visible to every local user. "--" ends the options all the same.
  function noticeNew(list) {
    var seen = Object.assign({}, root.seenArtifacts)
    var fresh = 0
    for (var i = 0; i < list.length; i++) {
      var key = list[i].id + "\n" + list[i].path + "\n" + list[i].mtime
      if (root.artifactsLoaded && !root.seenArtifacts[key] && list[i].isNew)
        fresh++
      seen[key] = true
    }
    root.seenArtifacts = seen
    root.artifactsLoaded = true
    if (fresh > 0)
      Quickshell.execDetached(["notify-send", "--app-name=Paddock", "--icon=folder-download", "--",
        strings.t("notice.title"), strings.tn("notice.body", fresh).arg(Commons.I18n.formatNumber(fresh))])
  }

  // Answers for a request, safest first. Enter alone never answers.
  function answersFor(a) {
    var list = ["deny", "once"]
    if (a && a.sessionAllowed) list.push("session")
    return list
  }
  function answer(a, decision) {
    runPaddock(["approvals", "answer", a.id, decision], function(code) {
      if (code !== 0) root.say(strings.t("approve.gone"))
      else root.say(decision === "deny" ? strings.t("approve.denied") : strings.t("approve.allowed"))
      root.actionIndex = -1
      root.refreshApprovals()
    })
  }
  // Give the keyboard to the approval card on screen, and get out of its way.
  function answerOnCard() {
    Quickshell.execDetached(["omarchy-shell", "harnessix.paddock.approvals", "keys"])
    root.close()
  }
  function refreshApprovals() {
    if (!approvalsProc.running) { approvalsProc.buffer = ""; approvalsProc.running = true }
  }
  function capabilityGlyph(c) {
    return ({ "screenshot": "󰹑", "exec": "󰆍", "fetch": "󰖟", "clipboard-read": "󰅇",
              "clipboard-write": "󰅌", "open-url": "󰌹", "notify": "󰂞", "spawn": "󰚩", "ask": "󰘥" })[c] || "󰀦"
  }
  // Seconds now, refreshed every second while requests wait, for countdowns.
  property real now: Date.now() / 1000
  Timer {
    interval: 1000
    running: root.approvals.length > 0
    repeat: true
    onTriggered: root.now = Date.now() / 1000
  }
  function timeLeft(a) {
    if (!(a.expires > 0)) return root.ago(a.created)
    var left = Math.max(0, Math.round(a.expires - root.now))
    return strings.tn("time.left", left).arg(Commons.I18n.formatNumber(left))
  }
  // A finished request, in the user's words. The log's decision text is
  // matched by its start, so new wordings fall back to a neutral label.
  function outcomeKey(decision, status) {
    var d = String(decision || "").toLowerCase()
    var st = String(status || "").toLowerCase()
    if (st.indexOf("time") >= 0 || d.indexOf("time") >= 0) return "timedOut"
    if (d.indexOf("invalid") === 0 || st === "invalid") return "invalid"
    if (d.indexOf("allowed by configuration") === 0) return "byConfig"
    if (d.indexOf("allowed for this session") === 0) return "session"
    if (d.indexOf("allowed") === 0) return "once"
    if (d.indexOf("disabled") >= 0 || (d.indexOf("configuration") >= 0 && d.indexOf("allow") < 0)) return "blocked"
    if (d.indexOf("denied") === 0) return "denied"
    return "other"
  }
  function ago(created) {
    var secs = Math.max(0, Math.floor(Date.now() / 1000 - Number(created || 0)))
    if (secs < 60) return strings.tn("time.secondsAgo", secs).arg(Commons.I18n.formatNumber(secs))
    var mins = Math.floor(secs / 60)
    if (mins < 60) return strings.tn("time.minutesAgo", mins).arg(Commons.I18n.formatNumber(mins))
    var hours = Math.floor(mins / 60)
    if (hours < 24) return strings.tn("time.hoursAgo", hours).arg(Commons.I18n.formatNumber(hours))
    var days = Math.floor(hours / 24)
    return strings.tn("time.daysAgo", days).arg(Commons.I18n.formatNumber(days))
  }


  function installedHarnesses() {
    return harnesses.filter(function(h) { return h.installed })
  }

  function refresh() {
    if (!psProc.running) { psProc.buffer = ""; psProc.running = true }
    refreshFiles()
    refreshKv()
  }

  function loadStartData() {
    if (!recentProc.running) { recentProc.buffer = ""; recentProc.running = true }
    if (!listProc.running) { listProc.buffer = ""; listProc.running = true }
  }

  function runPaddock(args, onDone) {
    var p = actionProc
    p.doneCallback = onDone || null
    p.command = ["sh", "-c", root.launcher, "paddock"].concat(args)
    p.running = true
  }

  // Row actions.
  function goToTerminal(s) {
    runPaddock(["focus", s.id], function(code) {
      if (code === 0) root.close()
      else root.say(strings.t("msg.noTerminal"))
    })
  }
  function showDisplay(s) {
    runPaddock(["focus", s.id, "--display"], function(code) {
      if (code === 0) root.close()
      else root.say(strings.t("msg.noDisplay"))
    })
  }
  function openFolder(s) {
    Quickshell.execDetached(["xdg-open", s.project])
    root.close()
  }
  function copyPath(s) {
    Quickshell.execDetached(["wl-copy", "--", s.project])
    root.say(strings.t("msg.copied"))
  }
  function stop(s) {
    if (root.confirmId !== s.id) {
      root.confirmId = s.id
      confirmTimer.restart()
      // The sandboxes that this one started stop with it.
      root.say(s.children > 0
        ? strings.tn("msg.confirmStopChildren", s.children).arg(s.harness).arg(s.projectName).arg(Commons.I18n.formatNumber(s.children))
        : strings.t("msg.confirmStop").arg(s.harness).arg(s.projectName))
      return
    }
    root.confirmId = ""
    runPaddock(["stop", s.id], function(code) {
      root.say(code === 0 ? strings.t("msg.stopped") : strings.t("msg.stopFailed"))
      root.refresh()
    })
  }
  // Freeze a running sandbox, or thaw a frozen one.
  function toggleFreeze(s) {
    var verb = s.frozenSince > 0 ? "thaw" : "freeze"
    runPaddock([verb, s.id], function(code) {
      root.say(code === 0
        ? strings.t(verb === "thaw" ? "msg.thawed" : "msg.frozen")
        : strings.t("msg.freezeFailed"))
      root.refresh()
    })
  }
  function runAction(s, name) {
    if (name === "folder") openFolder(s)
    else if (name === "freeze") toggleFreeze(s)
    else if (name === "copy") copyPath(s)
    else if (name === "display") showDisplay(s)
    else if (name === "stop") stop(s)
  }

  // New sandbox.
  function startNew() {
    root.view = "project"
    root.selectedIndex = 0
    root.actionIndex = -1
    root.confirmId = ""
    loadStartData()
  }
  // Any folder, not only recent ones. The panel steps aside while the
  // dialog is open and comes back at the harness step.
  function pickFolder() {
    if (folderProc.running) return
    root.controller.hide()
    folderProc.buffer = ""
    folderProc.running = true
  }
  function chooseProject(path) {
    root.chosenProject = path
    root.view = "harness"
    var list = installedHarnesses()
    var last = ""
    for (var i = 0; i < recentProjects.length; i++) if (recentProjects[i].project === path) last = recentProjects[i].harness
    root.selectedIndex = 0
    for (var j = 0; j < list.length; j++) if (list[j].name === last) root.selectedIndex = j
  }
  function launch(harness) {
    var entry = harnesses.filter(function(h) { return h.name === harness })[0]
    if (entry && entry.app) {
      // An app is not a terminal program: `paddock NAME` refuses it.
      Quickshell.execDetached(["sh", "-c", root.appLauncher, "paddock-app", harness, root.chosenProject])
      root.close()
      return
    }
    Quickshell.execDetached(["sh", "-c", root.terminal, "paddock-terminal", root.chosenProject, "paddock · " + harness + " · " + root.baseName(root.chosenProject), "sh", "-c", root.launcher, "paddock", harness])
    root.close()
  }
  function back() {
    root.confirmId = ""
    if (view === "key") { view = "keys"; kvEntry = null; return true }
    if (view === "keys") { view = "kv"; kvKeys = []; kvTarget = null; selectedIndex = 0; return true }
    if (view === "kv") { view = "list"; selectedIndex = 0; return true }
    if (view === "harness") { view = "project"; selectedIndex = 0; return true }
    if (view === "project") { view = "list"; selectedIndex = 0; return true }
    return false
  }

  // paddock kv. Names, paths and keys from paddock go to commands as
  // separate arguments after a check of their documented shape, and a key
  // comes after "--". Values never go into a command: a copy goes to wl-copy
  // on standard input.
  function kvSafeName(n) { return /^[a-z][a-z0-9-]{0,31}$/.test(n) || /^solo-[A-Za-z0-9_.-]{1,128}$/.test(n) }
  function kvSafeKey(k) { return typeof k === "string" && k.length > 0 && k.length <= 256 && !/[\u0000-\u001f\u007f]/.test(k) }
  function kvScopeArgs(t) {
    if (!t) return null
    if (t.kind === "user") return ["--user"]
    if (t.project === "" || t.project.charAt(0) !== "/") return null
    if (t.kind === "legacy") return ["--legacy", "--project", t.project]
    if (!kvSafeName(t.name)) return null
    return ["--pasture", t.name, "--project", t.project]
  }
  // Only while the panel is open: the bar shows nothing of paddock kv.
  function refreshKv(force) {
    if (!root.opened && !force) return
    if (!kvOverviewProc.running) { kvOverviewProc.buffer = ""; kvOverviewProc.running = true }
    if (view === "keys" || view === "key") refreshKvKeys()
  }
  function refreshKvKeys() {
    var args = kvScopeArgs(kvTarget)
    if (!args || kvListProc.running) return
    kvListProc.buffer = ""
    kvListProc.command = ["sh", "-c", root.launcher, "paddock", "kv", "list", "--json"].concat(args)
    kvListProc.running = true
  }
  function openKv() {
    root.view = "kv"
    root.selectedIndex = 0
    root.actionIndex = -1
    root.confirmId = ""
    refreshKv()
  }
  function openKvScope(t) {
    root.kvTarget = t
    root.kvKeys = []
    root.kvCollapsed = ({})
    root.view = "keys"
    root.selectedIndex = 0
    root.confirmId = ""
    refreshKvKeys()
  }
  function openKvKey(r) {
    var args = kvScopeArgs(kvTarget)
    if (!args || !kvSafeKey(r.key) || kvGetProc.running) return
    kvGetProc.buffer = ""
    kvGetProc.meta = r
    kvGetProc.command = ["sh", "-c", root.launcher, "paddock", "kv", "get", "--json"].concat(args, ["--", r.key])
    kvGetProc.running = true
  }
  function toggleKvGroup(prefix) {
    var c = Object.assign({}, root.kvCollapsed)
    c[prefix] = !c[prefix]
    root.kvCollapsed = c
  }
  function kvIsLease(key) { return String(key).indexOf("lease:") === 0 }
  function kvGroupOf(key) {
    var i = String(key).indexOf(":")
    return i > 0 ? String(key).slice(0, i + 1) : ""
  }
  // Keys by the part before the first ":", locks first, keys without a
  // prefix last. A closed group keeps its header row.
  function buildKvRows(keys, collapsed) {
    var groups = {}
    var order = []
    for (var i = 0; i < keys.length; i++) {
      var g = kvGroupOf(keys[i].key)
      if (!groups[g]) { groups[g] = []; order.push(g) }
      groups[g].push(keys[i])
    }
    order.sort(function(a, b) {
      if (a === "lease:") return -1
      if (b === "lease:") return 1
      if (a === "") return 1
      if (b === "") return -1
      return a < b ? -1 : (a > b ? 1 : 0)
    })
    var rows = []
    for (var j = 0; j < order.length; j++) {
      rows.push({ group: true, prefix: order[j], count: groups[order[j]].length, closed: collapsed[order[j]] === true })
      if (!collapsed[order[j]]) rows = rows.concat(groups[order[j]])
    }
    return rows
  }
  function kvSize(bytes) {
    var b = Number(bytes || 0)
    if (b < 1024) return strings.tn("kv.bytes", b).arg(Commons.I18n.formatNumber(b))
    return strings.t("kv.kib").arg(Commons.I18n.formatNumber(Math.round(b / 102.4) / 10))
  }
  function kvLeft(expires) {
    var secs = Math.max(0, Math.round(Number(expires || 0) - Date.now() / 1000))
    if (secs < 60) return strings.tn("time.left", secs).arg(Commons.I18n.formatNumber(secs))
    var mins = Math.round(secs / 60)
    if (mins < 60) return strings.tn("kv.minutesLeft", mins).arg(Commons.I18n.formatNumber(mins))
    var hours = Math.round(mins / 60)
    if (hours < 48) return strings.tn("kv.hoursLeft", hours).arg(Commons.I18n.formatNumber(hours))
    var days = Math.round(hours / 24)
    return strings.tn("kv.daysLeft", days).arg(Commons.I18n.formatNumber(days))
  }
  function kvWho(r) {
    return r.writer === "user" ? strings.t("kv.byYou") : r.writerHarness + " · " + r.writer
  }
  function kvScopeTitle(t) {
    if (!t) return ""
    if (t.kind === "user") return strings.t("kv.user")
    if (t.kind === "legacy") return strings.t("kv.legacy").arg(t.projectName)
    if (t.solo) return strings.t("kv.solo").arg(t.members.length ? t.members[0].harness : t.name)
    return t.name
  }
  function kvMembersText(t) {
    if (t.kind === "user") return strings.t("kv.userHint")
    if (t.kind === "legacy") return strings.t("kv.legacyHint")
    var on = t.members.filter(function(m) { return m.running })
    if (!on.length) return strings.t("kv.noMemberRuns")
    return strings.t("kv.running").arg(on.map(function(m) { return m.harness + (m.frozen ? " 󰏤" : "") }).join(", "))
  }
  // Delete a key, or free a lock. Like stop: the first press asks.
  function kvRemove(r) {
    var args = kvScopeArgs(kvTarget)
    if (!args || !kvSafeKey(r.key)) return
    var lease = kvIsLease(r.key)
    if (root.confirmId !== "kv:" + r.key) {
      root.confirmId = "kv:" + r.key
      confirmTimer.restart()
      root.say(lease ? strings.t("kv.confirmRelease").arg(r.key.slice(6)).arg(kvWho(r))
                     : strings.t("kv.confirmDelete").arg(r.key))
      return
    }
    root.confirmId = ""
    var cmd = lease ? ["kv", "release"].concat(args, ["--", r.key.slice(6)])
                    : ["kv", "delete"].concat(args, ["--", r.key])
    runPaddock(cmd, function(code) {
      root.say(code === 0 ? strings.t(lease ? "kv.released" : "kv.deleted") : strings.t("kv.removeFailed"))
      if (root.view === "key") { root.view = "keys"; root.kvEntry = null }
      root.refreshKvKeys()
      root.refreshKv()
    })
  }
  function kvCopy() {
    if (!root.kvEntry || kvCopyProc.running) return
    kvCopyProc.text = root.kvEntry.value
    kvCopyProc.running = true
  }

  // Footer and state actions.
  function runChecks() {
    Quickshell.execDetached(["env", "PADDOCK_HOLD=" + strings.t("terminal.pressEnter"), "sh", "-c", root.terminal, "paddock-terminal",
      Quickshell.env("HOME") || "/", "paddock doctor", "sh", "-c", root.holdOpen, "paddock-hold", "pd", "doctor"])
    root.close()
  }
  function install() {
    var dir = decodeURIComponent(String(Qt.resolvedUrl("..")).replace(/^file:\/\//, ""))
    Quickshell.execDetached(["env", "PADDOCK_HOLD=" + strings.t("terminal.pressEnter"), "sh", "-c", root.terminal, "paddock-terminal",
      dir, "paddock install", "sh", "-c", root.holdOpen, "paddock-hold", "bin/install"])
    root.close()
  }

  // Keyboard.
  function move(dy) {
    var n = rowCount()
    if (n === 0) return
    root.actionIndex = -1
    // The first key shows the cursor where it is, without moving it.
    if (!root.cursorActive) { root.cursorActive = true; return }
    root.selectedIndex = Math.max(0, Math.min(n - 1, root.selectedIndex + dy))
  }
  function moveAction(dx) {
    if (view !== "list" || rowCount() === 0) return
    root.cursorActive = true
    if (isApprovalRow(selectedIndex) || isKvRow(selectedIndex)) return
    var n = isArtifactRow(selectedIndex) ? 2 : actionsFor(sandboxAt(selectedIndex)).length
    root.actionIndex = Math.max(-1, Math.min(n - 1, root.actionIndex + dx))
  }
  function activate() {
    if (view === "project") {
      if (selectedIndex === 0) pickFolder()
      else chooseProject(recentProjects[selectedIndex - 1].project)
      return
    }
    if (view === "harness") {
      var list = installedHarnesses()
      if (list.length) launch(list[selectedIndex].name)
      return
    }
    if (view === "kv") { if (kvScopes.length) openKvScope(kvScopes[selectedIndex]); return }
    if (view === "keys") {
      var r = kvRows[selectedIndex]
      if (!r) return
      if (r.group) toggleKvGroup(r.prefix); else openKvKey(r)
      return
    }
    if (view === "key") return
    if (isApprovalRow(selectedIndex)) { answerOnCard(); return }
    if (isKvRow(selectedIndex)) { openKv(); return }
    if (isArtifactRow(selectedIndex)) {
      var f = artifactAt(selectedIndex)
      if (actionIndex === 1) revealFile(f); else recoverFile(f)
      return
    }
    if (sandboxes.length === 0) { startNew(); return }
    var s = sandboxAt(selectedIndex)
    if (actionIndex < 0) goToTerminal(s)
    else runAction(s, actionsFor(s)[actionIndex])
  }

  // `omarchy-shell harnessix.paddock.kv open` opens the panel at the shared
  // data, for a key binding.
  ShellIpc {
    target: "harnessix.paddock.kv"
    function open(): void { root.open(""); root.openKv() }
  }

  Component.onCompleted: refresh()

  Timer {
    interval: root.opened ? 2000 : root.refreshIntervalMs
    running: true
    repeat: true
    onTriggered: root.refresh()
  }
  Timer { id: confirmTimer; interval: 4000; onTriggered: root.confirmId = "" }
  Timer { id: messageTimer; interval: 4000; onTriggered: root.message = "" }

  component JsonProc: Process {
    id: proc
    property string buffer: ""
    stdout: SplitParser {
      onRead: function(line) { if (proc.buffer.length < 262144) proc.buffer += line }
    }
  }

  JsonProc {
    id: folderProc
    command: ["sh", "-c", "command -v zenity >/dev/null 2>&1 || exit 127; exec zenity --file-selection --directory --title=\"$1\" --filename=\"$HOME/\"", "paddock-folder", strings.t("new.chooseFolderTitle")]
    onExited: function(exitCode) {
      var path = buffer.trim()
      if (exitCode === 127) { root.controller.show(); root.say(strings.t("new.noZenity")); return }
      if (exitCode !== 0 || path === "") { root.controller.show(); return }
      root.chooseProject(path)
      root.controller.show()
    }
  }

  JsonProc {
    id: psProc
    command: ["sh", "-c", root.launcher, "paddock", "ps", "--json"]
    onExited: function(exitCode) {
      root.loaded = true
      if (exitCode === 127) { root.pluginState = "missing"; root.sandboxes = []; root.runningCount = 0; return }
      try {
        var parsed = JSON.parse(buffer)
        if (!parsed || parsed.api_version !== "paddock.ps/v1") throw "version"
        var raw = Array.isArray(parsed.sandboxes) ? parsed.sandboxes.slice(0, 32) : []
        var clean = []
        for (var i = 0; i < raw.length; i++) {
          var r = raw[i] || {}
          clean.push({
            id: root.plain(r.id, 64),
            harness: root.plain(r.harness, 24),
            project: root.plain(r.project, 512),
            projectName: root.plain(root.baseName(r.project), 48),
            backend: root.plain(r.backend, 12),
            gui: root.plain(r.gui, 12),
            network: root.plain(r.network, 12),
            auth: root.plain(r.auth || "", 40),
            started: Number(r.started || 0),
            frozenSince: Number(r.frozen_since || 0),
            // Its own scope (bwrap, srt), or a container that can pause.
            canFreeze: (typeof r.unit === "string" && r.unit !== "") || r.backend === "docker" || r.backend === "podman",
            windowPid: Number.isInteger(r.window_pid) && r.window_pid > 0 ? r.window_pid : 0,
            warning: Array.isArray(r.warnings) && r.warnings.length ? root.plain(r.warnings[0], 200) : "",
            // A sandbox that another sandbox started (paddock-host spawn).
            // paddock ps lists it right after its parent.
            parent: root.plain(r.parent || "", 64),
            pasture: root.plain(r.pasture || "", 32),
            parentHarness: "",
            depth: 0,
            children: 0
          })
        }
        for (var c = 0; c < clean.length; c++) {
          var p = clean[c]
          for (var d = 0; p.parent !== "" && d < 4; d++) {
            var up = clean.find(function(o) { return o.id === p.parent })
            if (!up) break
            if (d === 0) { clean[c].parentHarness = up.harness; up.children++ }
            clean[c].depth++
            p = up
          }
        }
        root.sandboxes = clean
        root.runningCount = clean.length
        root.pluginState = "ok"
        root.lastWarning = ""
        if (root.view === "list" && root.selectedIndex >= clean.length) root.selectedIndex = Math.max(0, clean.length - 1)
      } catch (e) {
        root.warnOnce("cannot read paddock ps --json (exit " + exitCode + "), showing an error: " + e)
        root.pluginState = "error"
      }
    }
  }

  JsonProc {
    id: filesProc
    command: ["sh", "-c", root.launcher, "paddock", "files", "--json"]
    onExited: function(exitCode) {
      // An older paddock has no files command: show nothing.
      if (exitCode !== 0) { root.artifacts = []; return }
      try {
        var p = JSON.parse(buffer)
        var raw = p && p.api_version === "paddock.files/v1" && Array.isArray(p.sandboxes) ? p.sandboxes.slice(0, 32) : []
        var clean = []
        for (var i = 0; i < raw.length; i++) {
          var s = raw[i] || {}
          var id = String(s.id || "")
          // ids are passed to a command; accept only the documented shape.
          if (!/^[A-Za-z0-9_-]{1,96}$/.test(id)) continue
          var files = Array.isArray(s.files) ? s.files : []
          for (var j = 0; j < files.length && clean.length < 40; j++) {
            var f = files[j] || {}
            var path = root.plain(f.path, 1024)
            if (path === "" || path.charAt(0) === "/" || path.split("/").indexOf("..") >= 0) continue
            clean.push({
              id: id,
              harness: root.plain(s.harness, 24),
              projectName: root.plain(root.baseName(s.project), 48),
              home: root.plain(s.home, 1024),
              path: path,
              name: root.plain(root.baseName(path), 120),
              target: root.plain(f.target, 1024),
              mtime: Number(f.mtime || 0),
              isNew: f["new"] === true
            })
          }
        }
        root.noticeNew(clean)
        root.artifacts = clean
        if (root.view === "list" && root.selectedIndex >= root.rowCount()) root.selectedIndex = Math.max(0, root.rowCount() - 1)
      } catch (e) {
        root.artifacts = []
      }
    }
  }

  JsonProc {
    id: recentProc
    command: ["sh", "-c", root.launcher, "paddock", "recent", "--json"]
    onExited: function(exitCode) {
      try {
        var p = JSON.parse(buffer)
        var raw = p && p.api_version === "paddock.recent/v1" && Array.isArray(p.projects) ? p.projects : []
        // Hidden folders (caches, scratch runs) stay out, as in the menu dialog.
        raw = raw.filter(function(r) { return typeof r.project === "string" && r.project.indexOf("/.") < 0 }).slice(0, 8)
        root.recentProjects = raw.map(function(r) {
          return { project: root.plain(r.project, 512), name: root.plain(root.baseName(r.project), 48), harness: root.plain(r.harness, 24), last: Number(r.last || 0) }
        })
      } catch (e) { root.recentProjects = [] }
      // Enter still starts the most recent project; the folder dialog is one row up.
      if (root.view === "project" && root.selectedIndex === 0 && root.recentProjects.length) root.selectedIndex = 1
    }
  }

  JsonProc {
    id: listProc
    command: ["sh", "-c", root.launcher, "paddock", "list", "--json"]
    onExited: function(exitCode) {
      try {
        var p = JSON.parse(buffer)
        var raw = p && p.api_version === "paddock.list/v1" && Array.isArray(p.harnesses) ? p.harnesses.slice(0, 32) : []
        root.harnesses = raw.map(function(h) {
          return { name: root.plain(h.name, 24), description: root.plain(h.description, 60), installed: h.installed === true, app: h.app === true }
        })
      } catch (e) { root.harnesses = [] }
    }
  }

  JsonProc {
    id: approvalsProc
    command: ["sh", "-c", root.launcher, "paddock", "approvals", "--json"]
    onExited: function(exitCode) {
      // An older paddock has no approvals command: show nothing.
      if (exitCode !== 0) { root.approvals = []; return }
      try {
        var p = JSON.parse(buffer)
        var raw = p && p.api_version === "paddock.approvals/v1" && Array.isArray(p.pending) ? p.pending.slice(0, 16) : []
        var clean = []
        for (var i = 0; i < raw.length; i++) {
          var r = raw[i] || {}
          var id = String(r.id || "")
          // ids are passed to a command; accept only the documented shape.
          if (!/^[A-Za-z0-9-]{1,96}$/.test(id)) continue
          clean.push({
            id: id,
            sandbox: root.plain(r.sandbox, 64),
            harness: root.plain(r.harness, 24),
            projectName: root.plain(root.baseName(r.project), 48),
            capability: root.plain(r.capability, 24),
            summary: root.plain(r.summary, 300),
            detail: root.plain(r.detail, 8000),
            reason: root.plain(r.reason, 1000),
            created: Number(r.created || 0),
            expires: Number(r.expires || 0),
            sessionAllowed: r.session_allowed === true,
            // The sandbox that started the asking sandbox, if any.
            startedBy: Array.isArray(r.started_by) && r.started_by.length ? root.plain(r.started_by[r.started_by.length - 1], 64) : ""
          })
        }
        root.approvals = clean
        if (root.view === "list" && root.selectedIndex >= root.rowCount()) root.selectedIndex = Math.max(0, root.rowCount() - 1)
      } catch (e) {
        root.approvals = []
      }
    }
  }

  JsonProc {
    id: logProc
    command: ["sh", "-c", root.launcher, "paddock", "log", "--json"]
    stdout: SplitParser {
      // One JSON object per line; keep the last lines only.
      onRead: function(line) {
        logProc.lines.push(line)
        if (logProc.lines.length > 200) logProc.lines.shift()
      }
    }
    property var lines: []
    onExited: function(exitCode) {
      var out = []
      for (var i = lines.length - 1; i >= 0 && out.length < 5; i--) {
        try {
          var r = JSON.parse(lines[i])
          if (!r || r.harness === "doctor") continue
          var d = root.plain(r.decision, 80)
          out.push({
            capability: root.plain(r.capability, 24),
            harness: root.plain(r.harness, 24),
            projectName: root.plain(root.baseName(r.project), 48),
            summary: root.plain(String(r.detail || "").split("\n").slice(-1)[0], 200),
            byAgent: d.indexOf("agent approver") >= 0,
            outcome: root.outcomeKey(d, root.plain(r.status, 40)),
            allowed: d.indexOf("allow") === 0 || d.indexOf("allowed") === 0,
            decision: d,
            time: Number(r.time || 0)
          })
        } catch (e) {
          root.debugLog("skipped an audit log line: " + e)
        }
      }
      root.recent = out
      lines = []
    }
  }

  // Requests time out in about two minutes, so they are checked often, also
  // while the panel is closed: the bar icon shows them.
  Timer {
    interval: root.opened ? 1500 : 3000
    running: root.pluginState !== "missing"
    repeat: true
    triggeredOnStart: true
    onTriggered: root.refreshApprovals()
  }

  JsonProc {
    id: kvOverviewProc
    command: ["sh", "-c", root.launcher, "paddock", "kv", "overview", "--json"]
    onExited: function(exitCode) {
      // An older paddock has no overview: show nothing.
      if (exitCode !== 0) { root.kvScopes = []; root.kvKeyTotal = 0; return }
      try {
        var p = JSON.parse(buffer)
        if (!p || p.api_version !== "paddock.kv/v1") throw "version"
        var rows = []
        var total = 0
        var raw = Array.isArray(p.pastures) ? p.pastures.slice(0, 64) : []
        for (var i = 0; i < raw.length; i++) {
          var r = raw[i] || {}
          var members = (Array.isArray(r.members) ? r.members.slice(0, 16) : []).map(function(m) {
            return { sandbox: root.plain(m.sandbox, 64), harness: root.plain(m.harness, 24),
                     running: m.running === true, frozen: Number(m.frozen_since || 0) > 0 }
          })
          var keys = Number(r.keys || 0)
          total += keys
          rows.push({ kind: "pasture", name: root.plain(r.name, 140), solo: r.solo === true,
                      project: root.plain(r.project, 1024), projectName: root.plain(root.baseName(r.project), 48),
                      keys: keys, leases: Number(r.leases || 0), members: members })
        }
        var userKeys = Number((p.user || {}).keys || 0)
        if (userKeys > 0) {
          total += userKeys
          rows.push({ kind: "user", name: "", solo: false, project: "", projectName: "", keys: userKeys,
                      leases: Number((p.user || {}).leases || 0), members: [] })
        }
        var legacy = Array.isArray(p.legacy) ? p.legacy.slice(0, 16) : []
        for (var j = 0; j < legacy.length; j++) {
          var l = legacy[j] || {}
          rows.push({ kind: "legacy", name: "", solo: false, project: root.plain(l.project, 1024),
                      projectName: root.plain(root.baseName(l.project), 48), keys: Number(l.keys || 0), leases: 0, members: [] })
        }
        root.kvScopes = rows
        root.kvKeyTotal = total
        if (root.view === "kv" && root.selectedIndex >= rows.length) root.selectedIndex = Math.max(0, rows.length - 1)
      } catch (e) {
        root.debugLog("cannot read paddock kv overview: " + e)
        root.kvScopes = []
      }
    }
  }

  JsonProc {
    id: kvListProc
    onExited: function(exitCode) {
      if (exitCode !== 0) { root.kvKeys = []; return }
      try {
        var p = JSON.parse(buffer)
        var raw = p && Array.isArray(p.entries) ? p.entries.slice(0, 500) : []
        var clean = []
        for (var i = 0; i < raw.length; i++) {
          var e = raw[i] || {}
          if (!root.kvSafeKey(e.key)) continue
          clean.push({ group: false, key: String(e.key), version: Number(e.version || 0), bytes: Number(e.bytes || 0),
                       writer: root.plain(e.writer, 64), writerHarness: root.plain(e.writer_harness, 24),
                       updated: Number(e.updated || 0), expires: Number(e.expires || 0) })
        }
        root.kvKeys = clean
        if (root.view === "keys" && root.selectedIndex >= root.rowCount()) root.selectedIndex = Math.max(0, root.rowCount() - 1)
      } catch (e2) {
        root.kvKeys = []
      }
    }
  }

  // One value, read when the user opens its key. The debug log never names it.
  JsonProc {
    id: kvGetProc
    property var meta: null
    onExited: function(exitCode) {
      if (exitCode !== 0) { root.say(strings.t("kv.readFailed")); root.refreshKvKeys(); return }
      try {
        var e = JSON.parse(buffer)
        var value = String(e.value === undefined || e.value === null ? "" : e.value)
        root.kvEntry = { key: String(e.key || meta.key), value: value, shown: root.plain(value, 4096),
                         cut: value.length > 4096, version: Number(e.version || 0), bytes: value.length,
                         writer: root.plain(e.writer, 64), writerHarness: root.plain(e.writer_harness, 24),
                         updated: Number(e.updated || 0), expires: Number(e.expires || 0) }
        root.view = "key"
        root.confirmId = ""
      } catch (e2) {
        root.say(strings.t("kv.readFailed"))
      }
      buffer = ""
    }
  }

  Process {
    id: kvCopyProc
    property string text: ""
    command: ["wl-copy"]
    stdinEnabled: true
    onStarted: {
      write(text)
      stdinEnabled = false
    }
    onExited: function(exitCode) {
      text = ""
      stdinEnabled = true
      root.say(exitCode === 0 ? strings.t("kv.copied") : strings.t("kv.copyFailed"))
    }
  }

  Process {
    id: actionProc
    property var doneCallback: null
    onExited: function(exitCode) { if (doneCallback) doneCallback(exitCode) }
  }

  component PlainText: Text {
    textFormat: Text.PlainText
    color: root.fg
    font.family: root.fontFamily
    font.pixelSize: Style.font.body
    elide: Text.ElideRight
  }

  // One row in any view: a glyph, a title, one or two caption lines, and
  // optional trailing actions.
  component ListRow: CursorSurface {
    id: row
    property int rowIndex: 0
    property string glyph: ""
    property color glyphColor: root.fg
    property string title: ""
    property string caption: ""
    property string caption2: ""
    property color caption2Color: root.dim
    property bool showActions: false
    // Levels below a parent row: a sandbox that another sandbox started.
    property int indent: 0
    default property alias actions: actionRow.data
    signal activated()

    readonly property bool selected: root.cursorActive && root.selectedIndex === rowIndex
    hasCursor: selected && root.actionIndex < 0
    // While the cursor is on one of its actions, the row keeps a quieter fill.
    current: selected && root.actionIndex >= 0
    foreground: root.fg
    fill: root.hoverFill
    currentFill: root.selectedFill
    width: parent ? parent.width : 0
    implicitHeight: body.implicitHeight + Style.spacing.rowPaddingX

    MouseArea {
      id: mouse
      anchors.fill: parent
      hoverEnabled: true
      cursorShape: Qt.PointingHandCursor
      onContainsMouseChanged: if (containsMouse) { root.cursorActive = true; root.selectedIndex = row.rowIndex; root.actionIndex = -1 }
      onClicked: row.activated()
    }

    Item {
      id: body
      anchors.left: parent.left
      anchors.right: parent.right
      anchors.verticalCenter: parent.verticalCenter
      anchors.leftMargin: Style.space(10 + 18 * Math.min(row.indent, 3))
      anchors.rightMargin: Style.space(8)
      implicitHeight: Math.max(texts.implicitHeight, actionRow.implicitHeight)

      PlainText {
        id: glyphText
        text: row.glyph
        color: row.glyphColor
        font.pixelSize: Style.font.heading
        // One column for all glyphs, so that the titles line up.
        width: Math.round(Style.font.heading * 1.4)
        horizontalAlignment: Text.AlignHCenter
        elide: Text.ElideNone
        anchors.left: parent.left
        anchors.verticalCenter: parent.verticalCenter
      }

      Column {
        id: texts
        spacing: Style.space(1)
        anchors.left: glyphText.right
        anchors.leftMargin: Style.space(10)
        anchors.right: actionRow.visible ? actionRow.left : parent.right
        anchors.rightMargin: actionRow.visible ? Style.space(6) : 0
        anchors.verticalCenter: parent.verticalCenter
        PlainText { width: parent.width; text: row.title }
        PlainText { width: parent.width; visible: text !== ""; text: row.caption; color: root.dim; font.pixelSize: Style.font.caption }
        PlainText { width: parent.width; visible: text !== ""; text: row.caption2; color: row.caption2Color; font.pixelSize: Style.font.caption }
      }

      RowLayout {
        id: actionRow
        visible: row.showActions && (mouse.containsMouse || row.selected)
        spacing: Style.space(2)
        anchors.right: parent.right
        anchors.verticalCenter: parent.verticalCenter
      }
    }
  }

  component ActionButton: PanelActionButton {
    property int sandboxIndex: 0
    property int position: 0
    foreground: root.fg
    fontFamily: root.fontFamily
    hasCursor: root.cursorActive && root.selectedIndex === sandboxIndex && root.actionIndex === position
    onHovered: function(isHovered) {
      if (isHovered) { root.cursorActive = true; root.selectedIndex = sandboxIndex; root.actionIndex = position }
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: root.anchorItem
    owner: root.barIdentity
    bar: root.bar
    open: root.opened
    focusTarget: panel.loadedContent
    contentWidth: panel.fittedContentWidth(Style.space(400))
    contentHeight: panel.fittedContentHeight(panel.loadedContentHeight, Style.space(520))

    content: PanelKeyCatcher {
      implicitHeight: col.implicitHeight
      anchors.fill: parent
      onMoveRequested: function(dx, dy) {
        if (dy !== 0) root.move(dy)
        else if (dx !== 0) root.moveAction(dx)
      }
      onActivateRequested: root.activate()
      onCloseRequested: if (!root.back()) root.close()
      onDeleteRequested: {
        if (root.view === "key") { if (root.kvEntry) root.kvRemove(root.kvEntry); return }
        if (root.view === "keys") {
          var kr = root.kvRows[root.selectedIndex]
          if (kr && !kr.group) root.kvRemove(kr)
          return
        }
        if (root.view !== "list" || root.rowCount() === 0) return
        if (root.isKvRow(root.selectedIndex)) return
        if (root.isApprovalRow(root.selectedIndex)) root.answer(root.approvals[root.selectedIndex], "deny")
        else if (root.isArtifactRow(root.selectedIndex)) return
        else root.stop(root.sandboxAt(root.selectedIndex))
      }
      onTextKey: function(text, modifiers) {
        if (root.view === "list" && (text === "n" || text === "N")) root.startNew()
        else if (text === "r" || text === "R") root.refresh()
        else if (root.view === "list" && root.kvVisible && (text === "k" || text === "K")) root.openKv()
        else if (root.view === "key" && (text === "c" || text === "C")) root.kvCopy()
      }

      ScrollView {
        id: scroll
        anchors.fill: parent
        clip: true
        ScrollBar.horizontal.policy: ScrollBar.AlwaysOff

        Column {
          id: col
          width: scroll.availableWidth
          spacing: Style.space(10)

          // Header: what is running, and the way to start something new.
          PanelHero {
            width: parent.width
            foreground: root.fg
            fontFamily: root.fontFamily
            iconComponent: Component { PlainText { text: "󱞚"; font.pixelSize: Style.font.display } }
            title: root.view === "project" ? strings.t("new.chooseProject")
              : root.view === "harness" ? strings.t("new.chooseHarness")
              : root.view === "kv" ? strings.t("kv.title")
              : root.view === "keys" ? root.kvScopeTitle(root.kvTarget)
              : root.view === "key" && root.kvEntry ? root.kvEntry.key
              : strings.t("panel.title")
            meta: {
              if (root.view === "project") return strings.t("new.stepProject")
              if (root.view === "harness") return strings.t("new.stepHarness")
              if (root.view === "kv")
                return (root.kvPastureCount > 0 ? strings.tn("kv.rowPastures", root.kvPastureCount).arg(Commons.I18n.formatNumber(root.kvPastureCount)) + " · " : "")
                  + strings.tn("kv.keys", root.kvKeyTotal).arg(Commons.I18n.formatNumber(root.kvKeyTotal))
              if (root.view === "keys" && root.kvTarget)
                return (root.kvTarget.projectName !== "" ? root.kvTarget.projectName + " · " : "")
                  + strings.tn("kv.keys", root.kvKeys.length).arg(Commons.I18n.formatNumber(root.kvKeys.length))
              if (root.view === "key" && root.kvEntry) return root.kvScopeTitle(root.kvTarget)
              if (root.pluginState === "missing") return strings.t("state.missingShort")
              if (root.pluginState === "error") return strings.t("state.errorShort")
              if (!root.loaded) return strings.t("state.loading")
              return root.runningCount === 0 ? strings.t("state.idle")
                : strings.tn("panel.running", root.runningCount).arg(Commons.I18n.formatNumber(root.runningCount))
            }
            trailingControl: Component {
              Button {
                visible: root.pluginState !== "missing"
                text: root.view === "list" ? strings.t("new.button") : strings.t("new.back")
                iconText: root.view === "list" ? "󰐕" : "󰁍"
                foreground: root.fg
                fontFamily: root.fontFamily
                onClicked: if (root.view === "list") root.startNew(); else root.back()
              }
            }
          }

          // Feedback for the last action.
          PlainText {
            width: parent.width
            visible: root.message !== ""
            text: root.message
            color: root.confirmId !== "" ? root.urgent : root.dim
            wrapMode: Text.WordWrap
            font.pixelSize: Style.font.caption
          }

          // Requests waiting for an answer. They are answered on the approval
          // card at the top of the screen; here, Enter hands it the keyboard.
          Column {
            width: parent.width
            spacing: Style.space(2)
            visible: root.view === "list" && root.approvals.length > 0

            PanelSectionHeader {
              width: parent.width
              text: strings.tn("approve.header", root.approvals.length).arg(Commons.I18n.formatNumber(root.approvals.length))
              foreground: root.urgent
              fontFamily: root.fontFamily
            }

            Repeater {
              model: root.view === "list" ? root.approvals : []
              delegate: ListRow {
                required property var modelData
                required property int index
                rowIndex: index
                glyph: root.capabilityGlyph(modelData.capability)
                glyphColor: root.urgent
                title: modelData.summary
                caption: strings.t("approve.meta").arg(modelData.harness).arg(modelData.projectName).arg(root.timeLeft(modelData))
                caption2: modelData.startedBy !== "" ? strings.t("approve.startedBy").arg(modelData.startedBy) : strings.t("approve.openCard")
                onActivated: root.answerOnCard()
              }
            }
          }

          // States without sandboxes.
          Column {
            width: parent.width
            spacing: Style.space(8)
            visible: root.view === "list" && (root.pluginState !== "ok" || (root.loaded && root.sandboxes.length === 0))
            PlainText {
              width: parent.width
              wrapMode: Text.WordWrap
              color: root.dim
              text: root.pluginState === "missing" ? strings.t("state.missing")
                : root.pluginState === "error" ? strings.t("state.error")
                : strings.t("state.empty")
            }
            Button {
              text: root.pluginState === "missing" ? strings.t("state.install") : strings.t("new.button")
              iconText: root.pluginState === "missing" ? "󰏔" : "󰐕"
              visible: root.pluginState !== "error"
              foreground: root.fg
              fontFamily: root.fontFamily
              onClicked: root.pluginState === "missing" ? root.install() : root.startNew()
            }
          }

          // Running sandboxes.
          Column {
            width: parent.width
            spacing: Style.space(2)
            visible: root.view === "list" && root.sandboxes.length > 0
            Repeater {
              model: root.view === "list" ? root.sandboxes : []
              delegate: ListRow {
                required property var modelData
                required property int index
                rowIndex: index + root.approvals.length
                indent: modelData.depth
                glyph: modelData.warning !== "" ? "󰀦" : (modelData.parent !== "" ? "󰚩" : (modelData.gui === "off" ? "󰆍" : (modelData.gui === "app" ? "󰀻" : "󰍹")))
                glyphColor: modelData.warning !== "" ? root.urgent : root.fg
                title: modelData.parent !== ""
                  ? strings.t("row.childTitle").arg(modelData.harness).arg(modelData.parentHarness || modelData.parent)
                  : strings.t("row.title").arg(modelData.harness).arg(modelData.projectName)
                caption: strings.t("row.detail").arg(modelData.backend).arg(modelData.network).arg(modelData.gui).arg(root.uptime(modelData.started))
                caption2: modelData.frozenSince > 0 ? strings.t("row.frozen").arg(root.uptime(modelData.frozenSince))
                  : modelData.warning !== "" ? modelData.warning
                  : (modelData.parent !== "" ? strings.t("row.startedBy").arg(modelData.parent)
                  : (modelData.auth !== "" && modelData.auth !== "shared" ? strings.t("row.auth").arg(modelData.auth)
                  : (modelData.pasture !== "" ? strings.t("row.pasture").arg(modelData.pasture) : "")))
                caption2Color: modelData.warning !== "" ? root.urgent : root.dim
                showActions: true
                onActivated: root.goToTerminal(modelData)

                ActionButton {
                  sandboxIndex: index + root.approvals.length; position: 0
                  iconText: "󰉋"; tooltipText: strings.t("action.folder")
                  onClicked: root.openFolder(modelData)
                }
                ActionButton {
                  sandboxIndex: index + root.approvals.length; position: 1
                  iconText: "󰆏"; tooltipText: strings.t("action.copy")
                  onClicked: root.copyPath(modelData)
                }
                ActionButton {
                  sandboxIndex: index + root.approvals.length; position: 2
                  visible: modelData.windowPid > 0
                  iconText: "󰖯"; tooltipText: strings.t("action.display")
                  onClicked: root.showDisplay(modelData)
                }
                ActionButton {
                  sandboxIndex: index + root.approvals.length; position: modelData.windowPid > 0 ? 3 : 2
                  visible: modelData.canFreeze
                  iconText: modelData.frozenSince > 0 ? "󰐊" : "󰏤"
                  tooltipText: modelData.frozenSince > 0 ? strings.t("action.thaw") : strings.t("action.freeze")
                  onClicked: root.toggleFreeze(modelData)
                }
                ActionButton {
                  sandboxIndex: index + root.approvals.length
                  position: (modelData.windowPid > 0 ? 3 : 2) + (modelData.canFreeze ? 1 : 0)
                  iconText: root.confirmId === modelData.id ? "󰗖" : "󰓛"
                  tooltipText: root.confirmId === modelData.id ? strings.t("action.confirmStop") : strings.t("action.stop")
                  foreground: root.confirmId === modelData.id ? root.urgent : root.fg
                  hoverColor: root.urgent
                  onClicked: root.stop(modelData)
                }
              }
            }
          }

          // Files that sandboxes saved outside the project: in the sandbox
          // home until recovered. Enter copies one to the host.
          Column {
            width: parent.width
            spacing: Style.space(2)
            visible: root.view === "list" && root.artifacts.length > 0

            PanelSectionHeader {
              width: parent.width
              text: strings.tn("files.header", root.artifacts.length).arg(Commons.I18n.formatNumber(root.artifacts.length))
              foreground: root.fg
              fontFamily: root.fontFamily
            }

            Repeater {
              model: root.view === "list" ? root.artifacts : []
              delegate: ListRow {
                required property var modelData
                required property int index
                readonly property int at: index + root.approvals.length + root.sandboxes.length
                rowIndex: at
                glyph: "󰈔"
                title: modelData.name + (modelData.isNew ? "  · " + strings.t("files.new") : "")
                caption: strings.t("files.caption").arg(modelData.harness).arg(modelData.projectName).arg(root.shortPath(root.dirName(modelData.target)))
                showActions: true
                onActivated: root.recoverFile(modelData)

                ActionButton {
                  sandboxIndex: at; position: 0
                  iconText: "󰇚"; tooltipText: strings.t("action.recover")
                  onClicked: root.recoverFile(modelData)
                }
                ActionButton {
                  sandboxIndex: at; position: 1
                  iconText: "󰉋"; tooltipText: strings.t("action.reveal")
                  onClicked: root.revealFile(modelData)
                }
              }
            }
          }

          // Shared data in paddock kv: one row that opens the kv view.
          Column {
            width: parent.width
            spacing: Style.space(2)
            visible: root.view === "list" && root.kvVisible
            ListRow {
              rowIndex: root.approvals.length + root.sandboxes.length + root.artifacts.length
              glyph: "󰆼"
              title: strings.t("kv.row")
              caption: (root.kvPastureCount > 0 ? strings.tn("kv.rowPastures", root.kvPastureCount).arg(Commons.I18n.formatNumber(root.kvPastureCount)) + " · " : "")
                + strings.tn("kv.keys", root.kvKeyTotal).arg(Commons.I18n.formatNumber(root.kvKeyTotal))
              onActivated: root.openKv()
            }
          }

          // The kv view: pastures, your own data, and data of older versions.
          Column {
            width: parent.width
            spacing: Style.space(2)
            visible: root.view === "kv"
            PlainText {
              width: parent.width
              wrapMode: Text.WordWrap
              color: root.dim
              font.pixelSize: Style.font.caption
              bottomPadding: Style.space(6)
              text: root.kvScopes.length === 0 ? strings.t("kv.empty") : strings.t("kv.intro")
            }
            Repeater {
              model: root.view === "kv" ? root.kvScopes : []
              delegate: ListRow {
                required property var modelData
                required property int index
                rowIndex: index
                glyph: modelData.kind === "user" ? "󰀄" : modelData.kind === "legacy" ? "󰁯" : (modelData.solo ? "󰆍" : "󰡉")
                glyphColor: modelData.kind === "legacy" ? root.dim : root.fg
                title: root.kvScopeTitle(modelData)
                caption: (modelData.projectName !== "" ? modelData.projectName + " · " : "")
                  + strings.tn("kv.keys", modelData.keys).arg(Commons.I18n.formatNumber(modelData.keys))
                  + (modelData.leases > 0 ? " · " + strings.tn("kv.locks", modelData.leases).arg(Commons.I18n.formatNumber(modelData.leases)) : "")
                caption2: root.kvMembersText(modelData)
                onActivated: root.openKvScope(modelData)
              }
            }
          }

          // The keys of one scope, by group. No value is read here.
          Column {
            width: parent.width
            spacing: Style.space(2)
            visible: root.view === "keys"
            PlainText {
              width: parent.width
              visible: root.kvKeys.length === 0
              wrapMode: Text.WordWrap
              color: root.dim
              text: strings.t("kv.noKeys")
            }
            Repeater {
              model: root.view === "keys" ? root.kvRows : []
              delegate: ListRow {
                required property var modelData
                required property int index
                rowIndex: index
                readonly property bool lease: !modelData.group && root.kvIsLease(modelData.key)
                glyph: modelData.group ? (modelData.closed ? "󰅂" : "󰅀") : (lease ? "󰌾" : "󰌆")
                glyphColor: root.confirmId === "kv:" + modelData.key ? root.urgent : root.fg
                title: modelData.group
                  ? (modelData.prefix === "lease:" ? strings.t("kv.groupLocks")
                     : modelData.prefix === "" ? strings.t("kv.groupOther") : modelData.prefix)
                    + "  " + Commons.I18n.formatNumber(modelData.count)
                  : (lease ? modelData.key.slice(6) : modelData.key)
                caption: modelData.group ? ""
                  : lease ? strings.t("kv.heldBy").arg(root.kvWho(modelData)).arg(root.kvLeft(modelData.expires))
                  : root.kvWho(modelData) + " · " + root.ago(modelData.updated) + " · " + root.kvSize(modelData.bytes)
                caption2: !modelData.group && !lease && modelData.expires > 0 ? root.kvLeft(modelData.expires) : ""
                onActivated: modelData.group ? root.toggleKvGroup(modelData.prefix) : root.openKvKey(modelData)
              }
            }
          }

          // One key: its value as plain text, and who wrote it.
          Column {
            width: parent.width
            spacing: Style.space(8)
            visible: root.view === "key" && root.kvEntry !== null
            PlainText {
              width: parent.width
              wrapMode: Text.WordWrap
              color: root.dim
              font.pixelSize: Style.font.caption
              text: root.kvEntry ? strings.t("kv.untrusted") : ""
            }
            PlainText {
              width: parent.width
              text: root.kvEntry ? root.kvEntry.shown : ""
              font.family: "monospace"
              font.pixelSize: Style.font.caption
              wrapMode: Text.WrapAnywhere
              elide: Text.ElideNone
              maximumLineCount: 40
            }
            PlainText {
              width: parent.width
              visible: root.kvEntry !== null && root.kvEntry.cut
              color: root.dim
              font.pixelSize: Style.font.caption
              text: strings.t("kv.cut")
            }
            PanelSeparator { width: parent.width; foreground: root.fg }
            PlainText {
              width: parent.width
              wrapMode: Text.WordWrap
              color: root.dim
              font.pixelSize: Style.font.caption
              text: root.kvEntry
                ? strings.t("kv.meta").arg(root.kvWho(root.kvEntry)).arg(root.ago(root.kvEntry.updated))
                    .arg(Commons.I18n.formatNumber(root.kvEntry.version)).arg(root.kvSize(root.kvEntry.bytes))
                  + (root.kvEntry.expires > 0 ? " · " + root.kvLeft(root.kvEntry.expires) : "")
                : ""
            }
            RowLayout {
              spacing: Style.space(6)
              Button {
                visible: root.kvEntry !== null && !root.kvIsLease(root.kvEntry.key)
                text: strings.t("kv.copy")
                iconText: "󰆏"
                foreground: root.fg
                fontFamily: root.fontFamily
                onClicked: root.kvCopy()
              }
              Button {
                visible: root.kvEntry !== null
                readonly property bool lease: root.kvEntry !== null && root.kvIsLease(root.kvEntry.key)
                readonly property bool asking: root.kvEntry !== null && root.confirmId === "kv:" + root.kvEntry.key
                text: asking ? strings.t("kv.confirm") : strings.t(lease ? "kv.release" : "kv.delete")
                iconText: lease ? "󰌿" : "󰆴"
                foreground: asking ? root.urgent : root.fg
                fontFamily: root.fontFamily
                onClicked: root.kvRemove(root.kvEntry)
              }
            }
          }

          // New sandbox, step 1: a recent project.
          Column {
            width: parent.width
            spacing: Style.space(2)
            visible: root.view === "project"
            PlainText {
              width: parent.width
              visible: root.recentProjects.length === 0
              wrapMode: Text.WordWrap
              color: root.dim
              text: strings.t("new.noRecent")
            }
            ListRow {
              rowIndex: 0
              glyph: "󰉖"
              title: strings.t("new.otherFolder")
              caption: strings.t("new.otherFolderHint")
              onActivated: root.pickFolder()
            }
            Repeater {
              model: root.view === "project" ? root.recentProjects : []
              delegate: ListRow {
                required property var modelData
                required property int index
                rowIndex: index + 1
                glyph: "󰉋"
                title: modelData.name
                caption: root.shortPath(modelData.project)
                caption2: strings.t("new.lastUsed").arg(modelData.harness)
                onActivated: root.chooseProject(modelData.project)
              }
            }
          }

          // New sandbox, step 2: a harness installed on this machine.
          Column {
            width: parent.width
            spacing: Style.space(2)
            visible: root.view === "harness"
            Repeater {
              model: root.view === "harness" ? root.installedHarnesses() : []
              delegate: ListRow {
                required property var modelData
                required property int index
                rowIndex: index
                glyph: modelData.app ? "󰀻" : "󰚩"
                title: modelData.name
                caption: modelData.description
                onActivated: root.launch(modelData.name)
              }
            }
          }

          // The last answers, so a decision made on the card or by the agent
          // approver can be checked later.
          Column {
            width: parent.width
            spacing: Style.space(4)
            visible: root.view === "list" && root.recent.length > 0
            PanelSectionHeader {
              width: parent.width
              text: strings.t("recent.header")
              foreground: root.fg
              fontFamily: root.fontFamily
            }
            Repeater {
              model: root.view === "list" ? root.recent : []
              delegate: RowLayout {
                required property var modelData
                width: parent.width
                spacing: Style.space(8)
                PlainText {
                  text: root.capabilityGlyph(modelData.capability)
                  color: modelData.allowed ? root.fg : root.dim
                  font.pixelSize: Style.font.body
                }
                PlainText {
                  Layout.fillWidth: true
                  text: modelData.summary
                  color: root.fg
                  font.pixelSize: Style.font.caption
                }
                PlainText {
                  text: strings.t("recent." + modelData.outcome)
                    + (modelData.byAgent ? " · " + strings.t("recent.byAgent") : "")
                    + " · " + root.ago(modelData.time)
                  color: modelData.allowed ? root.fg : root.dim
                  font.pixelSize: Style.font.caption
                }
              }
            }
          }

          PanelSeparator { width: parent.width; foreground: root.fg }

          // Footer: keyboard keys for this view, and the self-check.
          Item {
            width: parent.width
            implicitHeight: Math.max(keysText.implicitHeight, checksButton.implicitHeight)
            PlainText {
              id: keysText
              anchors.left: parent.left
              anchors.right: checksButton.left
              anchors.rightMargin: Style.space(8)
              anchors.verticalCenter: parent.verticalCenter
              color: root.dim
              font.pixelSize: Style.font.caption
              wrapMode: Text.WordWrap
              text: root.view === "kv" ? strings.t("keys.kv")
                : root.view === "keys" ? strings.t("keys.kvKeys")
                : root.view === "key" ? strings.t("keys.kvKey")
                : root.view !== "list" ? strings.t("keys.new")
                : root.isApprovalRow(root.selectedIndex) && root.approvals.length > 0 ? strings.t("keys.toCard")
                : root.isArtifactRow(root.selectedIndex) && root.artifacts.length > 0 ? strings.t("keys.files")
                : root.isKvRow(root.selectedIndex) ? strings.t("keys.kvRow")
                : strings.t("keys.list")
            }
            Button {
              id: checksButton
              anchors.right: parent.right
              anchors.verticalCenter: parent.verticalCenter
              visible: root.pluginState !== "missing"
              text: strings.t("footer.checks")
              iconText: "󰄬"
              foreground: root.fg
              fontFamily: root.fontFamily
              tooltipText: strings.t("footer.checksTip")
              onClicked: root.runChecks()
            }
          }
        }
      }
    }
  }
}
