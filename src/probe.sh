# paddock escape probe. It runs inside a sandbox and prints one line per check:
# "ok <name>", "FAIL <name>: <detail>" or "skip <name>: <reason>".
# Every "must not" check tries the thing an agent must not be able to do.

ok() { echo "ok $1"; }
fail() { echo "FAIL $1: $2"; }
skip() { echo "skip $1: $2"; }
has() { command -v "$1" >/dev/null 2>&1; }

P="$PADDOCK_PROBE_PROJECT"

if [ -f "$HOME/.paddock-doctor-canary" ]; then ok home-is-sandbox-home; else fail home-is-sandbox-home "HOME is not the sandbox home"; fi

leaks=""
old_ifs=$IFS; IFS=:
for d in $PADDOCK_PROBE_DENIED; do
  [ -e "$d" ] && leaks="$leaks $d"
done
IFS=$old_ifs
if [ -z "$leaks" ]; then ok protected-paths-absent; else fail protected-paths-absent "visible:$leaks"; fi

if [ -e "$PADDOCK_PROBE_OTHER" ]; then fail other-sandbox-home-hidden "$PADDOCK_PROBE_OTHER is visible"; else ok other-sandbox-home-hidden; fi

if [ -n "$PADDOCK_PROBE_HOSTFILE" ] && [ -e "$PADDOCK_PROBE_HOSTFILE" ]; then
  fail host-home-hidden "$PADDOCK_PROBE_HOSTFILE is visible"
else
  ok host-home-hidden
fi

if touch "$P/.git/hooks/pre-commit" 2>/dev/null; then fail git-hooks-read-only "created .git/hooks/pre-commit"; else ok git-hooks-read-only; fi
if (echo "[core]" >> "$P/.git/config") 2>/dev/null; then fail git-config-read-only "appended to .git/config"; else ok git-config-read-only; fi

if grep -q paddock-secret-canary "$P/.env" 2>/dev/null; then fail secrets-masked ".env content is readable"; else ok secrets-masked; fi
if (echo x > "$P/.env") 2>/dev/null; then fail secrets-read-only ".env is writable"; else ok secrets-read-only; fi

if [ "$PADDOCK_PROBE_ENV" = clean ]; then
  if [ -n "$DOCTOR_PLAIN_CANARY" ]; then fail host-env-not-inherited "a host variable passed in clean mode"; else ok host-env-not-inherited; fi
else
  if [ -n "$DOCTOR_PLAIN_CANARY" ]; then ok host-env-inherited; else fail host-env-inherited "an ordinary host variable did not pass"; fi
fi
if [ -n "$AWS_SECRET_ACCESS_KEY" ]; then fail cloud-credentials-dropped "AWS_SECRET_ACCESS_KEY is set"; else ok cloud-credentials-dropped; fi
if [ -n "$DOCTOR_CANARY_SOCKET$DOCTOR_CANARY_CTL" ]; then fail socket-variables-dropped "a host socket variable is set"; else ok socket-variables-dropped; fi

if [ -S /var/run/docker.sock ] || [ -S /run/docker.sock ]; then fail container-socket-absent "docker socket visible"; else ok container-socket-absent; fi

if [ -n "$PADDOCK_PROBE_HOSTPID" ] && [ -e "/proc/$PADDOCK_PROBE_HOSTPID" ] && [ "$PADDOCK_PROBE_HOSTPID" -gt 100 ]; then
  fail host-processes-hidden "host pid $PADDOCK_PROBE_HOSTPID is visible"
else
  ok host-processes-hidden
fi

n=$(grep -c : /etc/passwd 2>/dev/null)
if [ "$PADDOCK_PROBE_BACKEND" = srt ]; then
  skip host-users-hidden "srt cannot replace /etc/passwd ($n entries visible)"
elif [ "${n:-0}" -le 3 ]; then ok host-users-hidden; else fail host-users-hidden "/etc/passwd has $n entries"; fi

if [ -e "$PADDOCK_PROBE_HOSTTMP" ]; then fail host-tmp-hidden "a file in the host /tmp is visible"; else ok host-tmp-hidden; fi
if [ -S "$PADDOCK_PROBE_HOSTSOCK" ]; then fail host-tmp-sockets-hidden "a socket in the host /tmp is visible"; else ok host-tmp-sockets-hidden; fi

if has sudo; then
  if sudo -n true 2>/dev/null; then fail no-privilege-gain "sudo works"; else ok no-privilege-gain; fi
else
  ok no-privilege-gain
fi
if has unshare; then
  if unshare -U true 2>/dev/null; then fail no-nested-userns "unshare -U works"; else ok no-nested-userns; fi
else
  skip no-nested-userns "unshare is not installed"
fi

# Try to write next to HOME. doctor checks on the host that nothing arrived.
(echo probe > "$HOME/../paddock-escape-probe") 2>/dev/null

# Leave traps for the host, as an agent could between runs. doctor checks on
# the host that it neither follows nor opens them (names as in doctor.rs).
rm -f "$P/.envrc" "$P/.vscode/tasks.json"
mkfifo "$P/.envrc" 2>/dev/null
mkdir -p "$P/.vscode" && ln -s /dev/zero "$P/.vscode/tasks.json" 2>/dev/null
ln -s ../paddock-escape-link-dir "$HOME/.paddock-probe-dir-link" 2>/dev/null
ln -s ../paddock-escape-link-file "$HOME/.paddock-probe-file-link" 2>/dev/null

if [ "$PADDOCK_PROBE_NETWORK" != host ]; then
  if has bash; then
    if timeout 3 bash -c "exec 3<>/dev/tcp/127.0.0.1/$PADDOCK_PROBE_PORT" 2>/dev/null; then
      fail host-loopback-unreachable "connected to a host 127.0.0.1 listener"
    else
      ok host-loopback-unreachable
    fi
  else
    skip host-loopback-unreachable "bash is not installed"
  fi
fi

# Other addresses of the host (docker0, VPN, TUN): services bound to 0.0.0.0
# there must not be reachable either.
if [ "$PADDOCK_PROBE_NETWORK" != host ] && has bash; then
  reached=""
  for a in $PADDOCK_PROBE_LOCAL_ADDRS; do
    if timeout 3 bash -c "exec 3<>/dev/tcp/$a/$PADDOCK_PROBE_ANYPORT" 2>/dev/null; then reached="$reached $a"; fi
  done
  if [ -z "$reached" ]; then ok host-addresses-unreachable; else fail host-addresses-unreachable "reached a host service at:$reached"; fi
fi

# The LAN (here the default gateway): blocked unless [network] lan = true.
# A refused connection means the packet arrived, so it fails the check too.
if [ -n "$PADDOCK_PROBE_LAN" ] && [ -z "$PADDOCK_PROBE_RELAY" ] && [ "$PADDOCK_PROBE_NETWORK" != host ] && has bash; then
  err=$(timeout 5 bash -c "exec 3<>/dev/tcp/$PADDOCK_PROBE_LAN/80" 2>&1); rc=$?
  case "$rc:$err" in
    0:*|*refused*) fail lan-unreachable "reached $PADDOCK_PROBE_LAN" ;;
    124:*) skip lan-unreachable "no answer from $PADDOCK_PROBE_LAN within 5 seconds" ;;
    *) ok lan-unreachable ;;
  esac
fi

if [ -n "$PADDOCK_PROBE_RELAY" ]; then
  if has curl; then
    if curl -sS -o /dev/null --max-time 5 --noproxy '*' https://example.com 2>/dev/null; then
      fail direct-egress-blocked "a direct connection bypassed the host proxy path"
    else
      ok direct-egress-blocked
    fi
    if curl -s -o /dev/null --max-time 5 "http://127.0.0.1:$PADDOCK_PROBE_PORT/" 2>/dev/null \
      || curl -s -o /dev/null --max-time 5 -p -x "$HTTPS_PROXY" "http://127.0.0.1:$PADDOCK_PROBE_PORT/" 2>/dev/null; then
      fail proxy-refuses-host "the proxy reached a host loopback port"
    else
      ok proxy-refuses-host
    fi
    reached=""
    for a in $PADDOCK_PROBE_LOCAL_ADDRS; do
      if curl -s -o /dev/null --max-time 5 -p -x "$HTTPS_PROXY" "http://$a:$PADDOCK_PROBE_ANYPORT/" 2>/dev/null; then reached="$reached $a"; fi
    done
    if [ -z "$reached" ]; then ok proxy-refuses-host-addresses; else fail proxy-refuses-host-addresses "reached:$reached"; fi
    if [ -n "$PADDOCK_PROBE_LAN" ]; then
      code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 -p -x "$HTTPS_PROXY" "http://$PADDOCK_PROBE_LAN/" 2>/dev/null)
      case "$code" in 2*|3*|401|404) fail proxy-refuses-lan "reached $PADDOCK_PROBE_LAN (HTTP $code)" ;; *) ok proxy-refuses-lan ;; esac
    fi
  else
    skip direct-egress-blocked "curl is not installed"
  fi
fi

# The host passthrough: the client reaches the host, a denied request returns
# its exit code, protected paths are refused, and answers cannot be forged.
if command -v paddock-host >/dev/null 2>&1; then
  if paddock-host capabilities --json >/dev/null 2>&1; then ok host-client-connects; else fail host-client-connects "paddock-host capabilities failed"; fi
  paddock-host exec --reason "paddock doctor" -- true >/dev/null 2>&1; rc=$?
  if [ "$rc" = 3 ] || [ "$rc" = 4 ]; then ok host-request-denied; else fail host-request-denied "exit code $rc, expected 3 or 4"; fi
  first_denied=${PADDOCK_PROBE_DENIED%%:*}
  paddock-host fetch "$first_denied" >/dev/null 2>&1; rc=$?
  if [ "$rc" = 8 ]; then ok host-fetch-protected-refused; else fail host-fetch-protected-refused "exit code $rc, expected 8"; fi
  if [ -e "$PADDOCK_PROBE_APPROVALS" ]; then fail host-approvals-hidden "$PADDOCK_PROBE_APPROVALS is visible"; else ok host-approvals-hidden; fi
else
  fail host-client-connects "paddock-host is not on PATH"
fi

# Push and pull requests: no credentials inside, and every push goes to the
# host. doctor answers pushes to origin with "allow for this session" and
# checks on the host what each push asked for.
if [ -n "$PADDOCK_PROBE_PUSH" ]; then
  helpers=$(git config --get-all credential.helper 2>/dev/null | grep -v '^$')
  creds=""
  for f in "$HOME/.git-credentials" "$HOME/.config/gh/hosts.yml" "$HOME/.config/git/credentials"; do [ -e "$f" ] && creds="$creds $f"; done
  if [ -n "$helpers$creds$GH_TOKEN$GITHUB_TOKEN" ]; then fail no-git-credentials "found:${helpers:+ helper $helpers}$creds${GH_TOKEN:+ GH_TOKEN}${GITHUB_TOKEN:+ GITHUB_TOKEN}"; else ok no-git-credentials; fi
  g() { git -C "$P" -c user.name=probe -c user.email=probe@localhost "$@"; }
  g commit -q --allow-empty -m "paddock probe push" 2>/dev/null
  if g push -q origin HEAD:refs/heads/paddock-probe >/dev/null 2>&1; then ok git-push-through-host; else fail git-push-through-host "git push origin failed"; fi
  g commit -q --allow-empty -m "paddock probe push 2" 2>/dev/null
  if g push -q origin HEAD:refs/heads/paddock-probe >/dev/null 2>&1; then ok git-push-session-grant; else fail git-push-session-grant "the second push failed"; fi
  (cd "$P" && paddock-host git push -q --force origin HEAD~1:refs/heads/paddock-probe >/dev/null 2>&1)
  (cd "$P" && paddock-host git push -q --force origin HEAD~2:refs/heads/paddock-probe >/dev/null 2>&1)
  # .git/config is read-only here, so name the new remote another way.
  url=$(g remote get-url origin)
  (cd "$P" && GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=remote.sneaky.url GIT_CONFIG_VALUE_0="$url" paddock-host git push -q sneaky HEAD:refs/heads/sneaky >/dev/null 2>&1); rc=$?
  if [ "$rc" = 3 ]; then ok git-push-denied-exit-code; else fail git-push-denied-exit-code "exit code $rc, expected 3"; fi
  (cd "$P" && gh auth token >/dev/null 2>&1); rc=$?
  if [ "$rc" = 8 ]; then ok forge-refuses-auth; else fail forge-refuses-auth "exit code $rc, expected 8"; fi
fi

# Positive checks: the sandbox must still be useful.
if [ -n "$PADDOCK_PROBE_ONLINE" ] && [ -z "$PADDOCK_PROBE_RELAY" ] && [ "$PADDOCK_PROBE_BACKEND" != srt ]; then
  if getent hosts example.com >/dev/null 2>&1; then ok dns-works; else fail dns-works "cannot resolve example.com"; fi
fi
if [ "$PADDOCK_PROBE_NETWORK" = allowlist ] && has curl; then
  if curl -s -o /dev/null --max-time 10 https://github.com 2>/dev/null; then fail unlisted-domain-blocked "reached github.com"; else ok unlisted-domain-blocked; fi
fi
if [ -n "$PADDOCK_PROBE_ONLINE" ]; then
  if has curl; then
    if curl -sS -o /dev/null --max-time 15 https://example.com 2>/dev/null; then ok https-works; else fail https-works "cannot fetch https://example.com"; fi
  else
    skip https-works "curl is not installed"
  fi
fi
if touch "$P/paddock-write-probe" 2>/dev/null; then ok project-writable; rm -f "$P/paddock-write-probe"; else fail project-writable "cannot write the project"; fi
if git -C "$P" -c user.name=probe -c user.email=probe@localhost commit -q --allow-empty -m probe 2>/dev/null; then ok git-commit-works; else fail git-commit-works "git commit failed"; fi

# mise: the shared installs are read-only here, and with network a plain
# registry tool from mise.toml installed into them before the harness started.
# srt has no shared installs, and its allowlist blocks the download.
if [ -n "$PADDOCK_PROBE_MISE" ]; then
  if [ "$PADDOCK_PROBE_BACKEND" = srt ]; then
    skip mise-shared-installs-read-only "srt has no shared installs"
  elif [ -n "$MISE_SYSTEM_INSTALLS_DIR" ] && (: > "$MISE_SYSTEM_INSTALLS_DIR/paddock-probe-hook") 2>/dev/null; then
    rm -f "$MISE_SYSTEM_INSTALLS_DIR/paddock-probe-hook"
    fail mise-shared-installs-read-only "wrote into $MISE_SYSTEM_INSTALLS_DIR"
  else
    ok mise-shared-installs-read-only
  fi
  if [ -n "$PADDOCK_PROBE_ONLINE" ] && [ "$PADDOCK_PROBE_BACKEND" != srt ]; then
    v=$(cd "$P" && jq --version 2>/dev/null)
    if [ "$v" = jq-1.7.1 ]; then ok mise-tools-provisioned; else fail mise-tools-provisioned "jq from mise.toml: $v"; fi
    w=$(cd "$P" && mise which jq 2>/dev/null)
    case "$w" in
      "$MISE_SYSTEM_INSTALLS_DIR"/*) ok mise-tools-shared ;;
      *) fail mise-tools-shared "jq is not in the shared installs: $w" ;;
    esac
  fi
fi

if [ -n "$PADDOCK_PROBE_APP" ]; then
  if [ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]; then ok app-display-socket; else fail app-display-socket "no socket at $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY"; fi
  if has wayland-info; then
    privileged=$(wayland-info 2>/dev/null | grep -oE "zwlr_screencopy_manager_v1|ext_image_copy_capture_manager_v1|hyprland_toplevel_export_manager_v1|zwp_virtual_keyboard_manager_v1|zwlr_virtual_pointer_manager_v1|zwlr_data_control_manager_v1|ext_data_control_manager_v1|zwlr_foreign_toplevel_manager_v1|ext_foreign_toplevel_list_v1|hyprland_input_capture_manager_v1|hyprland_global_shortcuts_manager_v1" | sort -u | tr '\n' ' ')
    total=$(wayland-info 2>/dev/null | grep -c "interface:")
    if [ "${total:-0}" -gt 0 ] && [ -z "$privileged" ]; then ok app-privileged-protocols-hidden; else fail app-privileged-protocols-hidden "offered: ${privileged:-no connection}"; fi
  else
    skip app-privileged-protocols-hidden "wayland-info is not installed"
  fi
  if has grim; then
    if grim /tmp/app-probe.png 2>/dev/null; then fail app-no-screen-capture "grim captured the host screen"; else ok app-no-screen-capture; fi
  fi
  if has wtype; then
    # Press and release Shift only, so nothing is typed even if this failed.
    if wtype -P Shift_L -p Shift_L 2>/dev/null; then fail app-no-virtual-keyboard "wtype sent input"; else ok app-no-virtual-keyboard; fi
  fi
  if gdbus call --session -d org.freedesktop.portal.Desktop -o /org/freedesktop/portal/desktop -m org.freedesktop.DBus.Properties.Get org.freedesktop.portal.OpenURI version >/dev/null 2>&1; then ok app-portal-reachable; else fail app-portal-reachable "the OpenURI portal is not reachable"; fi
  if [ "$PADDOCK_PROBE_APP_BUS" = private ]; then
    # The bus is a dbus-daemon in this sandbox, not the host bus.
    bus_id=$(gdbus call --session -d org.freedesktop.DBus -o /org/freedesktop/DBus -m org.freedesktop.DBus.GetId 2>/dev/null | tr -d "(',)")
    bus_pid=$(gdbus call --session -d org.freedesktop.DBus -o /org/freedesktop/DBus -m org.freedesktop.DBus.GetConnectionUnixProcessID org.freedesktop.DBus 2>/dev/null | grep -oE "[0-9]+" | tail -n 1)
    if [ -z "$bus_id" ]; then
      fail app-bus-private "no session bus"
    elif [ "$bus_id" = "$PADDOCK_PROBE_HOST_BUS_ID" ]; then
      fail app-bus-private "the session bus is the host bus"
    elif [ -n "$bus_pid" ] && [ "$(cat /proc/$bus_pid/comm 2>/dev/null)" = dbus-daemon ]; then
      ok app-bus-private
    else
      fail app-bus-private "the bus daemon is not a process of this sandbox"
    fi
    # The keyring on the bus is the sandbox's own gnome-keyring.
    k_pid=$(gdbus call --session -d org.freedesktop.DBus -o /org/freedesktop/DBus -m org.freedesktop.DBus.GetConnectionUnixProcessID org.freedesktop.secrets 2>/dev/null | grep -oE "[0-9]+" | tail -n 1)
    case "$(cat /proc/${k_pid:-0}/comm 2>/dev/null)" in
      gnome-keyring*) ok app-keyring-private ;;
      *) fail app-keyring-private "no keyring of this sandbox on the bus" ;;
    esac
    if gdbus call --session -d org.freedesktop.Notifications -o /org/freedesktop/Notifications -m org.freedesktop.Notifications.GetServerInformation 2>/dev/null | grep -q "'paddock'"; then ok app-notifications-relayed; else fail app-notifications-relayed "the notification service is not paddock portal"; fi
    # A link goes to the host broker, which the doctor answers with deny:
    # the portal reports that as code 1.
    if has dbus-monitor; then
      dbus-monitor --session "type='signal',interface='org.freedesktop.portal.Request',member='Response'" >/tmp/probe-monitor 2>&1 &
      mon=$!
      sleep 0.3
      gdbus call --session -d org.freedesktop.portal.Desktop -o /org/freedesktop/portal/desktop -m org.freedesktop.portal.OpenURI.OpenURI "" "https://example.com/paddock-probe" "{'handle_token': <'probe'>}" >/dev/null 2>&1
      i=0; while ! grep -q "uint32" /tmp/probe-monitor && [ $i -lt 60 ]; do sleep 0.05; i=$((i+1)); done
      kill $mon 2>/dev/null
      if grep -A1 "path=/org/freedesktop/portal/desktop/request/.*/probe" /tmp/probe-monitor | grep -q "uint32 1"; then ok app-open-url-asks-host; else fail app-open-url-asks-host "OpenURI did not reach the host broker: $(tr '\n' ' ' </tmp/probe-monitor | cut -c1-200)"; fi
    else
      skip app-open-url-asks-host "dbus-monitor is not installed"
    fi
    if gdbus call --session -d org.freedesktop.portal.Desktop -o /org/freedesktop/portal/desktop -m org.freedesktop.DBus.Properties.Get org.freedesktop.portal.FileChooser version >/dev/null 2>&1 \
      || gdbus call --session -d org.freedesktop.portal.Documents -o /org/freedesktop/portal/documents -m org.freedesktop.DBus.Peer.Ping >/dev/null 2>&1; then
      fail app-no-file-portal "a file portal answered"
    else
      ok app-no-file-portal
    fi
  else
    if gdbus call --session -d org.freedesktop.secrets -o /org/freedesktop/secrets -m org.freedesktop.DBus.Peer.Ping >/dev/null 2>&1; then fail app-keyring-hidden "the host keyring answered"; else ok app-keyring-hidden; fi
  fi
  if gdbus call --session -d org.freedesktop.systemd1 -o /org/freedesktop/systemd1 -m org.freedesktop.DBus.Peer.Ping >/dev/null 2>&1; then fail app-systemd-hidden "the systemd user manager answered"; else ok app-systemd-hidden; fi
  if gdbus call --session -d org.freedesktop.Flatpak -o /org/freedesktop/Flatpak/Development -m org.freedesktop.DBus.Peer.Ping >/dev/null 2>&1; then fail app-flatpak-spawn-hidden "the flatpak host command service answered"; else ok app-flatpak-spawn-hidden; fi
fi

if [ -n "$PADDOCK_PROBE_GUI" ]; then
  if [ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]; then ok display-socket; else fail display-socket "no socket at $XDG_RUNTIME_DIR/$WAYLAND_DISPLAY; $(tail -n 3 /tmp/paddock-weston.log 2>/dev/null | tr '\n' ' ')"; fi
  out=$(paddock-screenshot /tmp/probe.png 2>&1)
  if [ -s /tmp/probe.png ]; then ok screenshot-works; else fail screenshot-works "$out"; fi
fi
