#!/usr/bin/env bash
echo `# <#` > /dev/null
# =============================================================================
#  balevpn — single-file installer + interactive manager for the Bale VPN
#  headless binary. This ONE file is a bash + PowerShell polyglot:
#
#    Linux / macOS :  bash balevpn.ps1            (or: chmod +x balevpn.ps1 ; ./balevpn.ps1)
#    Windows       :  .\balevpn.ps1               (Windows PowerShell 5.1 or PowerShell 7+)
#
#  How the polyglot works: bash runs the section below and `exit`s before it
#  ever reaches the PowerShell. PowerShell instead treats the whole bash
#  section as one big block comment (opened on the echo line at the very top)
#  and runs only the section below the closing marker further down. The bash
#  section must therefore never contain the comment-closing two-character
#  sequence, or PowerShell would stop skipping early.
#
#  What it does, in order:
#    1. Resolve a release version (latest, or --version vX.Y.Z).
#    2. If the headless binary isn't sitting next to this script, download the
#       right one for this OS/arch from the matching GitHub release.
#    3. If the management API isn't already answering on the port, start the
#       binary headless in the background and wait for it.
#    4. Drop into an interactive menu driving the same HTTP API the desktop GUI
#       uses: OTP sign-in, client peer-selection, server-mode admission, and an
#       Upgrade action that stops the running binary, re-downloads, and restarts.
#
#  bash requires: bash, curl, jq.   PowerShell requires: nothing extra.
# =============================================================================

set -uo pipefail

REPO="kookoo1sabzy/BaleVPN"
# ${BASH_SOURCE[0]} is unset when piped (curl … | bash) — and that's an
# unbound-variable error under `set -u`. Fall back to $0, and when there's no
# real path (piped, or run as a bare filename) use the current directory.
_src="${BASH_SOURCE[0]:-$0}"
case "$_src" in
  */*) SCRIPT_DIR="$(cd "$(dirname "$_src")" && pwd)" ;;
  *)   SCRIPT_DIR="$(pwd)" ;;
esac

# ── Defaults (override via flags) ───────────────────────────────────────────
PORT=3001
VERSION=""        # empty -> resolve "latest"
MODE=""           # empty -> leave to the menu (binary defaults to client)
NAT_MODE=""       # empty -> binary default (kernel on Unix); server-only
REINSTALL=0
DO_UPGRADE=0
DO_STOP=0
DO_RESTART=0
DO_LOGS=0

# ── Pretty output ───────────────────────────────────────────────────────────
if [ -t 1 ]; then
  C_RESET=$'\033[0m'; C_BOLD=$'\033[1m'; C_DIM=$'\033[2m'
  C_RED=$'\033[31m'; C_GRN=$'\033[32m'; C_YLW=$'\033[33m'; C_CYN=$'\033[36m'
else
  C_RESET=; C_BOLD=; C_DIM=; C_RED=; C_GRN=; C_YLW=; C_CYN=
fi
info()  { printf '%s\n' "${C_CYN}==>${C_RESET} $*"; }
ok()    { printf '%s\n' "${C_GRN}==>${C_RESET} $*"; }
warn()  { printf '%s\n' "${C_YLW}warning:${C_RESET} $*" >&2; }
die()   { printf '%s\n' "${C_RED}error:${C_RESET} $*" >&2; exit 1; }

usage() {
  cat <<EOF
${C_BOLD}balevpn${C_RESET} — install and configure the Bale VPN headless binary.

Usage: $0 [options]

Options:
  --version <vX.Y.Z>   Pin a release version (default: latest).
  --port <int>         HTTP management port (default: 3001).
  --mode <client|server>
                       Start in this mode. Omit to choose from the menu.
  --nat-mode <kernel|userspace>
                       Server forwarding mode, used only when launching the
                       binary. Pass 'userspace' for a no-root server.
  --upgrade            Stop the running binary, download the target version
                       (latest unless --version), and restart. Then open the menu.
  --reinstall          Stop, re-download the binary, and restart (then open the menu).
  --restart            Stop the running binary and start it again. Then open the menu.
  --stop, --kill       Stop the running binary and exit (do not start or open the menu).
  --logs               Tail the binary's log file and exit.
  -h, --help           Show this help.

A plain run (no command flags) never kills a running binary: if one is already
serving on the port it just attaches to it; otherwise it installs/starts one.
The binary is only stopped by --upgrade, --reinstall, --restart, or --stop.

The binary is installed next to this script:
  ${SCRIPT_DIR}/bale-vpn
EOF
}

# ── Arg parsing ─────────────────────────────────────────────────────────────
while [ $# -gt 0 ]; do
  case "$1" in
    --version)  VERSION="${2:?--version needs a value}"; shift 2 ;;
    --port)     PORT="${2:?--port needs a value}"; shift 2 ;;
    --mode)     MODE="${2:?--mode needs a value}"; shift 2 ;;
    --nat-mode) NAT_MODE="${2:?--nat-mode needs a value}"; shift 2 ;;
    --upgrade)  DO_UPGRADE=1; shift ;;
    --reinstall) REINSTALL=1; shift ;;
    --stop|--kill) DO_STOP=1; shift ;;
    --restart)  DO_RESTART=1; shift ;;
    --logs)     DO_LOGS=1; shift ;;
    -h|--help)  usage; exit 0 ;;
    *) die "unknown argument: $1 (try --help)" ;;
  esac
done

API="http://127.0.0.1:${PORT}"
BIN="${SCRIPT_DIR}/bale-vpn"
LOG="${SCRIPT_DIR}/bale-vpn.log"
PID_FILE="${SCRIPT_DIR}/bale-vpn.pid"
VERSION_FILE="${SCRIPT_DIR}/bale-vpn.version"

# ── Dependency check ────────────────────────────────────────────────────────
check_deps() {
  command -v curl >/dev/null 2>&1 || die "missing dependency: curl"
  if ! command -v jq >/dev/null 2>&1; then
    die "missing dependency: jq
  Install it with:  (Debian/Ubuntu) sudo apt-get install -y jq
                    (Fedora)        sudo dnf install -y jq
                    (macOS)         brew install jq"
  fi
}

# ── OS / arch -> release asset name ─────────────────────────────────────────
asset_name() {
  local os arch
  os="$(uname -s)"; arch="$(uname -m)"
  case "$os" in
    Linux)
      case "$arch" in
        x86_64|amd64) echo "bale-vpn-headless-linux-x86_64" ;;
        *) die "no prebuilt Linux binary for arch '$arch' (only x86_64 is released)" ;;
      esac ;;
    Darwin)
      case "$arch" in
        arm64|aarch64) echo "bale-vpn-headless-macos-aarch64" ;;
        x86_64|amd64)  echo "bale-vpn-headless-macos-x86_64" ;;
        *) die "no prebuilt macOS binary for arch '$arch'" ;;
      esac ;;
    *) die "unsupported OS '$os' — on Windows run this file with PowerShell" ;;
  esac
}

installed_version() { cat "$VERSION_FILE" 2>/dev/null || echo "unknown"; }

# ── Resolve "latest" tag from the GitHub API ────────────────────────────────
resolve_version() {
  if [ -n "$VERSION" ]; then echo "$VERSION"; return; fi
  local tag
  tag="$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" \
           | jq -r '.tag_name // empty')"
  [ -n "$tag" ] || die "could not resolve the latest release tag from GitHub"
  echo "$tag"
}

# ── Install the binary next to this script if absent (or --reinstall) ───────
ensure_binary() {
  if [ -x "$BIN" ] && [ "$REINSTALL" -eq 0 ]; then
    info "binary already present: ${C_DIM}${BIN}${C_RESET} (version $(installed_version))"
    return
  fi
  local tag asset url tmp
  tag="$(resolve_version)"
  asset="$(asset_name)"
  url="https://github.com/${REPO}/releases/download/${tag}/${asset}"
  info "downloading ${C_BOLD}${asset}${C_RESET} (${tag})"
  tmp="$(mktemp "${SCRIPT_DIR}/.bale-vpn.XXXXXX")"
  if ! curl -fSL --progress-bar -o "$tmp" "$url"; then
    rm -f "$tmp"
    die "download failed: $url
  (check the version exists, or pass --version with a valid tag)"
  fi
  chmod +x "$tmp"
  # Strip macOS Gatekeeper quarantine so it runs without a prompt.
  if [ "$(uname -s)" = "Darwin" ]; then
    xattr -d com.apple.quarantine "$tmp" 2>/dev/null || true
  fi
  mv -f "$tmp" "$BIN"
  printf '%s\n' "$tag" > "$VERSION_FILE"
  ok "installed ${tag} -> ${BIN}"
}

# ── Is the HTTP API already answering on our port? ──────────────────────────
api_up() { curl -fsS -o /dev/null --max-time 2 "${API}/state" 2>/dev/null; }

# ── Stop any running instance of our binary ─────────────────────────────────
# Must work from a *fresh* script run against a daemon a previous run left
# behind: discover the process by pidfile AND by binary path (pgrep), then
# escalate SIGTERM -> wait-until-API-down -> SIGKILL, and verify it's gone.
stop_running() {
  local pids=() pid p i
  if [ -f "$PID_FILE" ]; then
    pid="$(cat "$PID_FILE" 2>/dev/null || true)"
    [ -n "$pid" ] && pids+=("$pid")
  fi
  # Catches instances we didn't launch, or where the pidfile was lost/stale.
  if command -v pgrep >/dev/null 2>&1; then
    while IFS= read -r p; do
      [ -n "$p" ] && [ "$p" != "$$" ] && pids+=("$p")
    done < <(pgrep -f "$BIN" 2>/dev/null || true)
  fi
  # De-duplicate.
  local uniq=() seen
  for pid in ${pids[@]+"${pids[@]}"}; do
    seen=0
    for p in ${uniq[@]+"${uniq[@]}"}; do [ "$p" = "$pid" ] && seen=1; done
    [ "$seen" -eq 0 ] && uniq+=("$pid")
  done

  if [ ${#uniq[@]} -eq 0 ]; then
    rm -f "$PID_FILE"
    api_up && warn "a bale-vpn API is up on ${API} but no matching process was found to stop"
    return 0
  fi

  info "stopping bale-vpn (pids: ${uniq[*]})"
  kill -TERM "${uniq[@]}" 2>/dev/null || true
  # The API going away is the real signal the port is released.
  for i in $(seq 1 20); do api_up || break; sleep 0.25; done
  # Escalate to SIGKILL for anything still alive.
  for pid in "${uniq[@]}"; do
    kill -0 "$pid" 2>/dev/null && { kill -KILL "$pid" 2>/dev/null || true; }
  done
  for i in $(seq 1 8); do api_up || break; sleep 0.25; done
  rm -f "$PID_FILE"
  if api_up; then
    warn "bale-vpn API still answering on ${API} after kill — another process may hold the port"
  else
    ok "stopped bale-vpn"
  fi
}

# ── Start the binary headless if nothing is serving on the port ─────────────
ensure_running() {
  if api_up; then
    info "management API already up on ${API}"
    return
  fi
  local args=(--headless --port "$PORT")
  # The binary uses clap subcommands (server / client), not a --mode flag,
  # and --nat-mode lives under `server`. Translate the script's flags.
  # With no mode, the binary parks at the UI picker and the menu sets the
  # mode via /config later.
  case "$MODE" in
    server) args+=(server); [ -n "$NAT_MODE" ] && args+=(--nat-mode "$NAT_MODE") ;;
    client) args+=(client) ;;
    "")     [ -n "$NAT_MODE" ] && warn "--nat-mode is ignored without --mode server" ;;
    *)      die "invalid --mode '$MODE' (use client or server)" ;;
  esac
  info "starting bale-vpn ${C_DIM}(${args[*]})${C_RESET}"
  nohup "$BIN" "${args[@]}" >"$LOG" 2>&1 &
  local pid=$!
  echo "$pid" > "$PID_FILE"
  local i
  for i in $(seq 1 30); do
    if api_up; then ok "management API up on ${API} (pid ${pid}, logs: ${LOG})"; return; fi
    if ! kill -0 "$pid" 2>/dev/null; then
      die "bale-vpn exited during startup — see ${LOG}"
    fi
    sleep 0.5
  done
  die "bale-vpn did not open its API within 15s — see ${LOG}"
}

# ── Upgrade: stop, re-download target version, restart ──────────────────────
do_upgrade() {
  local ver="${1:-}"
  [ -n "$ver" ] && VERSION="$ver"
  info "upgrading${VERSION:+ to ${VERSION}}…"
  stop_running
  REINSTALL=1
  ensure_binary
  REINSTALL=0
  ensure_running
  ok "upgrade complete (now $(installed_version))"
}

# ── HTTP helpers (endpoints return 200 even on logical failure) ─────────────
# --max-time caps every call so a slow endpoint (e.g. /server/pending while the
# server is still starting after a mode switch) can't freeze the live redraw.
api_get()    { curl -fsS --max-time 10 "${API}$1"; }
api_post()   { curl -fsS --max-time 10 -X POST -H 'Content-Type: application/json' -d "${2:-{}}" "${API}$1"; }
api_delete() { curl -fsS --max-time 10 -X DELETE "${API}$1"; }

# Read from /dev/tty, not stdin, so prompts still work when the script is
# piped (curl … | bash) — there stdin is the script text, not the keyboard.
prompt() { local __v; read -e -r -p "$1" __v </dev/tty; printf '%s' "$__v"; }
pause()  { read -r -p "${C_DIM}(press enter)${C_RESET} " _ </dev/tty || true; }

# Read one possibly-very-long line, byte by byte, bypassing the terminal's
# canonical-mode line cap (~1KB on macOS) that otherwise swallows the Enter
# key when a long JWT is pasted. Silent; also strips bracketed-paste guards
# some terminals wrap pasted text in.
read_line() {
  local line='' c
  while IFS= read -rsn1 c; do
    case $c in ''|$'\n'|$'\r') break ;; esac
    line+=$c
  done </dev/tty
  line=${line//$'\e[200~'/}
  line=${line//$'\e[201~'/}
  printf '%s' "$line"
}

# ── Auth (SMS OTP) ──────────────────────────────────────────────────────────
do_login() {
  echo
  info "${C_BOLD}Sign in${C_RESET} — Bale SMS one-time-password"
  local phone resp thash registered code verify needs name
  phone="$(prompt 'Phone number (e.g. 989123456789): ')"
  [ -n "$phone" ] || { warn "no phone entered"; return 1; }

  resp="$(api_post /auth/start "$(jq -nc --arg p "$phone" '{phone:$p}')")" \
    || { warn "auth/start request failed"; return 1; }
  if [ "$(jq -r '.ok' <<<"$resp")" != "true" ]; then
    warn "auth/start: $(jq -r '.error // "unknown error"' <<<"$resp")"; return 1
  fi
  thash="$(jq -r '.transactionHash' <<<"$resp")"
  registered="$(jq -r '.isRegistered' <<<"$resp")"
  ok "code sent. (registered account: ${registered})"

  code="$(prompt 'Enter the SMS code: ')"
  verify="$(api_post /auth/verify \
    "$(jq -nc --arg t "$thash" --arg c "$code" --argjson r "$registered" \
        '{transactionHash:$t, code:$c, isRegistered:$r}')")" \
    || { warn "auth/verify request failed"; return 1; }
  if [ "$(jq -r '.ok' <<<"$verify")" != "true" ]; then
    warn "verify failed: $(jq -r '.error // "unknown error"' <<<"$verify")"; return 1
  fi
  needs="$(jq -r '.needsSignup' <<<"$verify")"
  if [ "$needs" = "true" ]; then
    info "new account — choose a display name"
    name="$(prompt 'Display name: ')"
    thash="$(jq -r '.transactionHash' <<<"$verify")"
    verify="$(api_post /auth/signup \
      "$(jq -nc --arg t "$thash" --arg n "$name" '{transactionHash:$t, name:$n}')")" \
      || { warn "auth/signup request failed"; return 1; }
    if [ "$(jq -r '.ok' <<<"$verify")" != "true" ]; then
      warn "signup failed: $(jq -r '.error // "unknown error"' <<<"$verify")"; return 1
    fi
  fi
  ok "signed in."
}

paste_token() {
  echo
  info "Paste a Bale access_token cookie (DevTools -> Application -> Cookies)."
  local t
  printf 'access_token: '
  t="$(read_line)"
  echo
  [ -n "$t" ] || { warn "empty token"; return 1; }
  info "received ${#t} characters"
  if api_post /connect "$(jq -nc --arg t "$t" '{token:$t}')" >/dev/null; then
    ok "token saved + connecting."; return 0
  fi
  warn "connect failed"; return 1
}

# Re-auth chooser: SMS OTP or paste a token (the signed-in menu's option 3).
reauth() {
  echo
  cat <<EOF
${C_BOLD}Re-authenticate${C_RESET}
  1) Sign in with SMS code
  2) Paste an access_token cookie
  3) Cancel
EOF
  case "$(prompt 'choice> ')" in
    1) do_login || pause ;;
    2) paste_token || pause ;;
    *) : ;;
  esac
}

# ── Live status banner (re-fetched on every TUI redraw) ─────────────────────
status_banner() {
  local st mode tok name wsr rooms peerid peername socks rdy who line2
  printf '\033[K\n'
  if ! st="$(api_get /state 2>/dev/null)"; then
    printf '  %sBaleVPN%s  ·  %s  ·  %s● API not running%s\033[K\n' \
      "$C_BOLD" "$C_RESET" "$(installed_version)" "$C_YLW" "$C_RESET"
    printf '\033[K\n'
    return 0
  fi
  mode="$(jq -r '.mode // "—"' <<<"$st")"
  tok="$(jq -r '.tokenSet' <<<"$st")"
  name="$(jq -r '.self.name // ""' <<<"$st")"
  wsr="$(jq -r '.wsReady' <<<"$st")"
  rooms="$(jq -r '.lkRooms' <<<"$st")"
  peerid="$(jq -r '.serverPeer.id // ""' <<<"$st")"
  peername="$(jq -r '.serverPeer.name // ""' <<<"$st")"
  socks="$(jq -r '.socks5Port // 0' <<<"$st")"
  rdy="$(jq -r '.clientRoomReady' <<<"$st")"
  if [ "$tok" = true ]; then who="${name:-signed in}"; else who="not signed in"; fi
  printf '  %sBaleVPN%s  ·  %s  ·  %s  ·  WS:%s\033[K\n' \
    "$C_BOLD" "$C_RESET" "$who" "$(installed_version)" "$wsr"
  local pend=0
  case "$mode" in
    client) line2="mode:client  peer:$([ -n "$peerid" ] && echo "${peername:-?}" || echo none)  up:$rdy"
            [ "$socks" != 0 ] && line2="$line2  socks:127.0.0.1:$socks" ;;
    server) pend="$(api_get /server/pending 2>/dev/null | jq 'length' 2>/dev/null || echo 0)"
            line2="mode:server  clients:$rooms  pending:${pend:-0}" ;;
    *)      line2="mode:(unset)" ;;
  esac
  printf '  %s%s%s\033[K\n' "$C_DIM" "$line2" "$C_RESET"
  # Live alert: new admission requests show up here on their own (the banner
  # re-fetches every refresh tick), so the operator notices without digging in.
  if [ "${pend:-0}" -gt 0 ] 2>/dev/null; then
    printf '  %s● %s pending admission request(s) — open "Pending requests"%s\033[K\n' \
      "$C_YLW" "$pend" "$C_RESET"
  fi
  printf '\033[K\n'
}

# ── Arrow-key menu ──────────────────────────────────────────────────────────
TUI_REFRESH="${TUI_REFRESH:-3}"   # seconds between idle status refreshes
TUI_INDEX=0                       # set by tui_menu: chosen 0-based index, -1 = back

# Always restore the cursor on exit (the menu hides it while navigating).
trap 'printf "\033[?25h\033[0m" >/dev/tty 2>/dev/null' EXIT

# tui_menu <status_fn> <title> <label…>
# Draws <status_fn> then a navigable list. ↑/↓ (or k/j) move, Enter selects,
# digits 1-9 jump-select, q / Esc / ← go back. Re-renders every TUI_REFRESH
# seconds while idle so the status banner stays live. Result in TUI_INDEX.
tui_menu() {
  local status_fn="$1" title="$2"; shift 2
  local labels=("$@") n=$# sel=0 key rest i banner now last
  # Fetch the status banner ONCE and cache it; navigation redraws from the
  # cache (instant, no network), and we only re-fetch on the idle timer tick.
  # Calling the API on every keypress is what made navigation feel laggy.
  banner="$("$status_fn" 2>/dev/null)"
  last=$(date +%s)
  printf '\033[?25l' >/dev/tty
  while :; do
    {
      printf '\033[H'
      printf '%s\n' "$banner"
      printf '  %s%s%s\033[K\n' "$C_BOLD" "$title" "$C_RESET"
      for i in "${!labels[@]}"; do
        if [ "$i" -eq "$sel" ]; then
          printf '   \033[7m %s \033[0m\033[K\n' "${labels[$i]}"
        else
          printf '     %s\033[K\n' "${labels[$i]}"
        fi
      done
      printf '   %s↑/↓ move · Enter select · ← back · ● live%s\033[K\n' "$C_DIM" "$C_RESET"
      printf '\033[J'
    } >/dev/tty
    IFS= read -rsn1 -t "$TUI_REFRESH" key </dev/tty || { key=__timeout__; }
    # Re-fetch the banner only on the timer (idle tick or once TUI_REFRESH
    # seconds have elapsed since the last fetch) — never per keystroke.
    now=$(date +%s)
    if [ "$key" = __timeout__ ] || [ $((now - last)) -ge "$TUI_REFRESH" ]; then
      banner="$("$status_fn" 2>/dev/null)"; last=$now
    fi
    case "$key" in
      __timeout__) : ;;                       # idle tick → re-render (refresh)
      $'\033')
        # Esc and arrows both start with ESC. Read the rest of the sequence:
        # arrows are ESC [ A/B/C/D and their 2 bytes are already buffered, so
        # this returns instantly. A *bare* Esc has nothing following, so on
        # macOS bash 3.2 (integer-only `read -t`) it settles after ~1s = Back.
        # Left-arrow (←) is the instant Back alternative.
        IFS= read -rsn2 -t 1 rest </dev/tty || rest=''
        case "$rest" in
          '[A'|'OA') sel=$(( (sel - 1 + n) % n )) ;;
          '[B'|'OB') sel=$(( (sel + 1) % n )) ;;
          '[D'|'OD'|'') TUI_INDEX=-1; break ;;   # left-arrow / bare Esc = back
          *) : ;;
        esac ;;
      ''|$'\n'|$'\r') TUI_INDEX=$sel; break ;;
      k|K) sel=$(( (sel - 1 + n) % n )) ;;
      j|J) sel=$(( (sel + 1) % n )) ;;
      [0-9]) if [ "$key" -ge 1 ] && [ "$key" -le "$n" ]; then TUI_INDEX=$((key-1)); break; fi ;;
    esac
  done
  printf '\033[?25h' >/dev/tty
}

# Run a leaf action on a cleared screen (cursor shown), then wait for a key
# so single-line results stay visible before the menu redraws.
tui_begin() { printf '\033[?25h\033[H\033[J' >/dev/tty; }
tui_end()   { printf '\n' >/dev/tty; read -rsn1 -p "  (press any key)" _ </dev/tty || true; }

set_mode() {
  api_post /config "$(jq -nc --arg m "$1" '{mode:$m}')" >/dev/null \
    && ok "mode -> $1" || warn "could not set mode"
}

# ── Client menu ─────────────────────────────────────────────────────────────
client_menu() {
  local q res
  while :; do
    tui_menu status_banner "Client" \
      "List contacts and pick a server peer" \
      "Search a contact by phone, then pick" \
      "Refresh contacts" \
      "Disconnect current peer"
    case "$TUI_INDEX" in
      0) tui_begin; pick_peer "$(api_get /peers | jq -c '.peers')" ;;
      1) tui_begin
         q="$(prompt 'phone to search: ')"
         res="$(api_post /contacts/search "$(jq -nc --arg q "$q" '{query:$q}')" | jq -c '.users')"
         pick_peer "$res" ;;
      2) tui_begin; api_post /peers/refresh >/dev/null && ok "refreshed" || warn "failed" ;;
      3) tui_begin; api_post /tunnel/disconnect >/dev/null && ok "disconnected" || warn "failed" ;;
      -1) return ;;
    esac
  done
}

# Argument: a JSON array of {id,name}. Arrow-select a peer to connect to.
pick_peer() {
  local arr="$1" labels=() line pid
  while IFS= read -r line; do labels+=("$line"); done \
    < <(jq -r '.[] | "\(.name // "(no name)")   [\(.id)]"' <<<"$arr")
  if [ "${#labels[@]}" -eq 0 ]; then tui_begin; warn "no contacts found"; tui_end; return; fi
  tui_menu status_banner "Pick a server peer  (Enter = connect · ← back)" "${labels[@]}"
  [ "$TUI_INDEX" -ge 0 ] || return
  pid="$(jq -r --argjson i "$TUI_INDEX" '.[$i].id // empty' <<<"$arr")"
  [ -n "$pid" ] || return
  tui_begin
  api_post /config "$(jq -nc --arg p "$pid" '{serverPeerId:$p}')" >/dev/null \
    && ok "connecting to peer ${pid}…" || warn "could not set peer"
  tui_end
}

# ── Server menu ─────────────────────────────────────────────────────────────
server_menu() {
  while :; do
    tui_menu status_banner "Server" \
      "Pending requests (allow / reject)" \
      "Allow-list (auto-accept callers)" \
      "Block-list (silently rejected callers)" \
      "Max simultaneous clients" \
      "Connected clients (view / disconnect)" \
      "Connect WS (start accepting calls)" \
      "Disconnect WS (stop, drop all clients)"
    case "$TUI_INDEX" in
      0) tui_begin; pending_menu ;;
      1) tui_begin; caller_list_menu "Allow-list" /server/admission ;;
      2) tui_begin; caller_list_menu "Block-list" /server/blacklist ;;
      3) tui_begin; max_clients_menu ;;
      4) tui_begin; connected_menu ;;
      5) tui_begin; api_post /connect >/dev/null && ok "WS connecting" || warn "connect failed" ;;
      6) tui_begin; api_post /disconnect >/dev/null && ok "WS disconnected" || warn "disconnect failed" ;;
      -1) return ;;
    esac
  done
}

pending_menu() {
  local rows labels=() line cid
  while :; do
    rows="$(api_get /server/pending)"
    labels=()
    while IFS= read -r line; do labels+=("$line"); done \
      < <(jq -r '.[] | "\(.callerName // "(unknown)")  [\(.callerId)]"' <<<"$rows")
    if [ "${#labels[@]}" -eq 0 ]; then tui_begin; info "no pending requests"; tui_end; return; fi
    tui_menu status_banner "Pending requests  (Enter = decide · ← back)" "${labels[@]}"
    [ "$TUI_INDEX" -ge 0 ] || return
    cid="$(jq -r --argjson i "$TUI_INDEX" '.[$i].callerId // empty' <<<"$rows")"
    [ -n "$cid" ] || continue
    tui_menu status_banner "Caller $cid" \
      "Allow once" \
      "Allow always (add to allow-list)" \
      "Reject (block this caller)"
    case "$TUI_INDEX" in
      0) api_post "/server/pending/${cid}/accept" '{"addToList":false}' >/dev/null ;;
      1) api_post "/server/pending/${cid}/accept" '{"addToList":true}'  >/dev/null ;;
      2) api_post "/server/pending/${cid}/reject" >/dev/null ;;
      *) : ;;
    esac
  done
}

# Args: title, base-path (/server/admission or /server/blacklist).
# First row is "Add"; Enter on a caller removes it; ← / Esc goes back.
caller_list_menu() {
  local title="$1" base="$2" rows labels=() line cid
  while :; do
    rows="$(api_get "$base")"
    labels=("+  Add a caller id")
    while IFS= read -r line; do labels+=("$line"); done \
      < <(jq -r '.[] | "\(.callerName // "(unknown)")  [\(.callerId)]"' <<<"$rows")
    tui_menu status_banner "$title  (Enter on a caller = remove · ← back)" "${labels[@]}"
    [ "$TUI_INDEX" -ge 0 ] || return
    if [ "$TUI_INDEX" -eq 0 ]; then
      tui_begin
      cid="$(prompt 'caller id to add (Bale numeric uid, blank to cancel): ')"
      [ -n "$cid" ] && { api_post "$base" "$(jq -nc --arg c "$cid" '{callerId:$c}')" >/dev/null && ok "added $cid" || warn "failed"; }
      tui_end
    else
      cid="$(jq -r --argjson i "$((TUI_INDEX-1))" '.[$i].callerId // empty' <<<"$rows")"
      [ -n "$cid" ] || continue
      tui_begin
      api_delete "${base}/${cid}" >/dev/null && ok "removed $cid" || warn "failed"
      tui_end
    fi
  done
}

max_clients_menu() {
  local cur val
  cur="$(api_get /server/max-clients)"
  echo
  info "current cap: $(jq -r '.value' <<<"$cur")  (max $(jq -r '.max' <<<"$cur"))"
  val="$(prompt 'new value (blank to keep): ')"
  [ -n "$val" ] || return
  api_post /server/max-clients "$(jq -nc --argjson v "$val" '{value:$v}')" >/dev/null \
    && ok "cap -> $val" || warn "could not set"
}

connected_menu() {
  local rows labels=() line cid
  while :; do
    rows="$(api_get /tunnel/clients)"
    labels=()
    while IFS= read -r line; do labels+=("$line"); done \
      < <(jq -r '.[] | "\(.callerName // "(unknown)")  [\(.callerId)]  rx=\(.rxBytes) tx=\(.txBytes)"' <<<"$rows")
    if [ "${#labels[@]}" -eq 0 ]; then tui_begin; info "no clients connected"; tui_end; return; fi
    tui_menu status_banner "Connected clients  (Enter = disconnect · ← back)" "${labels[@]}"
    [ "$TUI_INDEX" -ge 0 ] || return
    cid="$(jq -r --argjson i "$TUI_INDEX" '.[$i].callerId // empty' <<<"$rows")"
    [ -n "$cid" ] || return
    tui_begin
    api_post "/tunnel/clients/${cid}/disconnect" >/dev/null && ok "disconnected $cid" || warn "failed"
    tui_end
  done
}

# ── Main loop ───────────────────────────────────────────────────────────────
quit_clean() { printf '\033[?25h\033[H\033[J' >/dev/tty 2>/dev/null; exit 0; }

main_menu() {
  local st tok mode auto_entered=0
  while :; do
    if ! st="$(api_get /state 2>/dev/null)"; then
      # Binary not running (killed or stopped) — offer to (re)start it.
      tui_menu status_banner "Binary not running" \
        "Start it" \
        "Upgrade binary (download + start)" \
        "Quit"
      case "$TUI_INDEX" in
        0) tui_begin; ensure_binary; ensure_running; tui_end ;;
        1) tui_begin; do_upgrade "$(prompt 'version (blank = latest): ')"; tui_end ;;
        2|-1) quit_clean ;;
      esac
      continue
    fi
    tok="$(jq -r '.tokenSet' <<<"$st")"
    mode="$(jq -r '.mode // ""' <<<"$st")"

    if [ "$tok" != true ]; then
      tui_menu status_banner "Not signed in" \
        "Sign in with SMS code" \
        "Paste an access_token cookie" \
        "Upgrade binary" \
        "Stop the binary" \
        "Quit (leave it running)"
      case "$TUI_INDEX" in
        0) tui_begin; do_login || pause ;;
        1) tui_begin; paste_token || pause ;;
        2) tui_begin; do_upgrade "$(prompt 'version (blank = latest): ')"; tui_end ;;
        3) tui_begin; stop_running ;;
        4|-1) quit_clean ;;
      esac
      continue
    fi

    # On startup, if a mode is already configured, jump straight into it.
    # After the user backs out (q) we fall through to the main menu so they
    # can switch modes.
    if [ "$auto_entered" -eq 0 ]; then
      auto_entered=1
      case "$mode" in
        client) client_menu; continue ;;
        server) server_menu; continue ;;
      esac
    fi

    tui_menu status_banner "Main" \
      "Client mode (connect to a server)" \
      "Server mode (share your connection)" \
      "Re-authenticate / paste token" \
      "Upgrade binary (stops + restarts it)" \
      "Stop the binary" \
      "Quit (leave it running)"
    case "$TUI_INDEX" in
      0) [ "$mode" = client ] || { tui_begin; set_mode client; }; client_menu ;;
      1) [ "$mode" = server ] || { tui_begin; set_mode server; }; server_menu ;;
      2) tui_begin; reauth ;;
      3) tui_begin; do_upgrade "$(prompt 'version (blank = latest): ')"; tui_end ;;
      4) tui_begin; stop_running ;;
      5|-1) quit_clean ;;
    esac
  done
}

# ── Go ──────────────────────────────────────────────────────────────────────
if [ "$DO_LOGS" -eq 1 ]; then
  [ -f "$LOG" ] || die "no log file yet at ${LOG} (has the binary been started?)"
  info "tailing ${LOG} — Ctrl-C to stop"
  exec tail -n 200 -f "$LOG"
fi

check_deps
if [ "$DO_STOP" -eq 1 ]; then
  stop_running
  exit 0
elif [ "$DO_RESTART" -eq 1 ]; then
  stop_running
  ensure_binary
  ensure_running
elif [ "$DO_UPGRADE" -eq 1 ]; then
  do_upgrade ""
elif [ "$REINSTALL" -eq 1 ]; then
  # A reinstall is only useful if the freshly downloaded binary actually
  # runs, so it stops + re-downloads + restarts (not just overwrite-on-disk).
  do_upgrade ""
else
  ensure_binary
  ensure_running
fi
main_menu
exit $?

#> > $null
# =============================================================================
#  PowerShell section (Windows). bash never reaches this; PowerShell skips
#  everything above via the block comment opened on the first `echo` line.
# =============================================================================

$ErrorActionPreference = 'Stop'

# Windows PowerShell 5.1 defaults to TLS 1.0/1.1; GitHub requires 1.2+. No-op
# on PowerShell 7 (which negotiates modern TLS on its own).
try { [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 } catch {}

$Repo = 'kookoo1sabzy/BaleVPN'
# $PSScriptRoot is empty when run via `irm … | iex` (no script file); fall back
# to the invocation path, then to the current directory.
$ScriptDir =
  if ($PSScriptRoot) { $PSScriptRoot }
  elseif ($MyInvocation.MyCommand.Path) { Split-Path -Parent $MyInvocation.MyCommand.Path }
  else { (Get-Location).Path }

# ── Defaults (override via flags) ────────────────────────────────────────────
$Port      = 3001
$Version   = ''
$Mode      = ''
$NatMode   = ''
$Reinstall = $false
$DoUpgrade = $false
$DoStop    = $false
$DoRestart = $false
$DoLogs    = $false

$IsWin = $IsWindows -or ($env:OS -eq 'Windows_NT')

# ── Pretty output ────────────────────────────────────────────────────────────
function Info($m) { Write-Host "==> $m" -ForegroundColor Cyan }
function Ok($m)   { Write-Host "==> $m" -ForegroundColor Green }
function Warn($m) { Write-Host "warning: $m" -ForegroundColor Yellow }
function Die($m)  { Write-Host "error: $m" -ForegroundColor Red; exit 1 }

function Show-Usage {
@"
balevpn — install and configure the Bale VPN headless binary.

Usage: balevpn.ps1 [options]

Options:
  --version <vX.Y.Z>   Pin a release version (default: latest).
  --port <int>         HTTP management port (default: 3001).
  --mode <client|server>
                       Start in this mode. Omit to choose from the menu.
  --nat-mode <kernel|userspace>
                       Server forwarding mode, used only when launching the binary.
  --upgrade            Stop the running binary, download the target version, restart.
  --reinstall          Stop, re-download the binary, and restart.
  --restart            Stop the running binary and start it again.
  --stop, --kill       Stop the running binary and exit (do not start).
  --logs               Tail the binary's log file and exit.
  -h, --help           Show this help.

A plain run (no command flags) never kills a running binary: if one is already
serving on the port it just attaches; otherwise it installs/starts one. The
binary is only stopped by --upgrade, --reinstall, --restart, or --stop.

The binary is installed next to this script:
  $ScriptDir\bale-vpn.exe
"@ | Write-Host
}

# ── Arg parsing ──────────────────────────────────────────────────────────────
for ($i = 0; $i -lt $args.Count; $i++) {
  switch ($args[$i]) {
    '--version'   { $i++; $Version = $args[$i] }
    '--port'      { $i++; $Port = [int]$args[$i] }
    '--mode'      { $i++; $Mode = $args[$i] }
    '--nat-mode'  { $i++; $NatMode = $args[$i] }
    '--upgrade'   { $DoUpgrade = $true }
    '--reinstall' { $Reinstall = $true }
    '--restart'   { $DoRestart = $true }
    '--stop'      { $DoStop = $true }
    '--kill'      { $DoStop = $true }
    '--logs'      { $DoLogs = $true }
    '-h'          { Show-Usage; exit 0 }
    '--help'      { Show-Usage; exit 0 }
    default       { Die "unknown argument: $($args[$i]) (try --help)" }
  }
}

$ApiBase     = "http://127.0.0.1:$Port"
$BinName     = if ($IsWin) { 'bale-vpn.exe' } else { 'bale-vpn' }
$Bin         = Join-Path $ScriptDir $BinName
$LogFile     = Join-Path $ScriptDir 'bale-vpn.log'
$PidFile     = Join-Path $ScriptDir 'bale-vpn.pid'
$VersionFile = Join-Path $ScriptDir 'bale-vpn.version'

# ── OS / arch -> release asset name ──────────────────────────────────────────
function Get-AssetName {
  if ($IsWin) { return 'bale-vpn-headless-windows-x86_64.exe' }
  $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
  if ($IsMacOS) {
    if ("$arch" -eq 'Arm64') { return 'bale-vpn-headless-macos-aarch64' }
    return 'bale-vpn-headless-macos-x86_64'
  }
  return 'bale-vpn-headless-linux-x86_64'
}

function Get-InstalledVersion {
  if (Test-Path $VersionFile) { (Get-Content $VersionFile -Raw).Trim() } else { 'unknown' }
}

# ── Resolve "latest" tag from the GitHub API ─────────────────────────────────
function Resolve-Version {
  if ($Version) { return $Version }
  $r = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" `
                         -Headers @{ 'User-Agent' = 'balevpn' }
  if (-not $r.tag_name) { Die 'could not resolve the latest release tag from GitHub' }
  return $r.tag_name
}

# ── Install the binary next to this script if absent (or -Reinstall) ─────────
function Install-Binary {
  if ((Test-Path $Bin) -and -not $Reinstall) {
    Info "binary already present: $Bin (version $(Get-InstalledVersion))"
    return
  }
  $tag   = Resolve-Version
  $asset = Get-AssetName
  $url   = "https://github.com/$Repo/releases/download/$tag/$asset"
  Info "downloading $asset ($tag)"
  $tmp = "$Bin.download"
  try {
    Invoke-WebRequest -Uri $url -OutFile $tmp -UseBasicParsing
  } catch {
    Remove-Item $tmp -ErrorAction SilentlyContinue
    Die "download failed: $url`n  (check the version exists, or pass --version with a valid tag)"
  }
  Move-Item -Force $tmp $Bin
  Set-Content -Path $VersionFile -Value $tag
  Ok "installed $tag -> $Bin"
}

# ── Is the HTTP API already answering on our port? ───────────────────────────
function Test-Api {
  try { Invoke-RestMethod -Uri "$ApiBase/state" -TimeoutSec 2 | Out-Null; return $true }
  catch { return $false }
}

# ── Stop any running instance of our binary ──────────────────────────────────
function Stop-Running {
  # Discover by pidfile AND by process name, so a fresh run can stop a daemon
  # an earlier run left behind, then verify the API is actually gone.
  $procs = @()
  if (Test-Path $PidFile) {
    $procId = (Get-Content $PidFile -Raw -ErrorAction SilentlyContinue).Trim()
    if ($procId -match '^\d+$') { $procs += Get-Process -Id ([int]$procId) -ErrorAction SilentlyContinue }
  }
  $procs += Get-Process -Name 'bale-vpn' -ErrorAction SilentlyContinue
  $procs = @($procs | Where-Object { $_ } | Sort-Object Id -Unique)

  if ($procs.Count -eq 0) {
    Remove-Item $PidFile -ErrorAction SilentlyContinue
    if (Test-Api) { Warn "a bale-vpn API is up on $ApiBase but no matching process was found to stop" }
    return
  }

  Info "stopping bale-vpn (pids: $($procs.Id -join ', '))"
  $procs | Stop-Process -Force -ErrorAction SilentlyContinue
  for ($i = 0; $i -lt 28; $i++) { if (-not (Test-Api)) { break }; Start-Sleep -Milliseconds 250 }
  Remove-Item $PidFile -ErrorAction SilentlyContinue
  if (Test-Api) { Warn "bale-vpn API still answering on $ApiBase after kill — another process may hold the port" }
  else { Ok 'stopped bale-vpn' }
}

# ── Start the binary headless if nothing is serving on the port ──────────────
function Start-IfNeeded {
  if (Test-Api) { Info "management API already up on $ApiBase"; return }
  $argList = @('--headless', '--port', "$Port")
  # The binary uses clap subcommands (server / client), not a --mode flag,
  # and --nat-mode lives under `server`. With no mode it parks at the UI
  # picker and the menu sets the mode via /config later.
  switch ($Mode) {
    'server' { $argList += 'server'; if ($NatMode) { $argList += @('--nat-mode', $NatMode) } }
    'client' { $argList += 'client' }
    ''       { if ($NatMode) { Warn '--nat-mode is ignored without --mode server' } }
    default  { Die "invalid --mode '$Mode' (use client or server)" }
  }
  Info "starting bale-vpn ($($argList -join ' '))"
  # env_logger writes to stderr, so stderr is the real log -> send it to the
  # main log file (what --logs tails); stdout (rarely used) goes to .out.
  $p = Start-Process -FilePath $Bin -ArgumentList $argList -PassThru -WindowStyle Hidden `
                     -RedirectStandardError $LogFile -RedirectStandardOutput "$LogFile.out"
  Set-Content -Path $PidFile -Value $p.Id
  for ($i = 0; $i -lt 30; $i++) {
    if (Test-Api) { Ok "management API up on $ApiBase (pid $($p.Id), logs: $LogFile)"; return }
    if ($p.HasExited) { Die "bale-vpn exited during startup — see $LogFile" }
    Start-Sleep -Milliseconds 500
  }
  Die "bale-vpn did not open its API within 15s — see $LogFile"
}

# ── Upgrade: stop, re-download target version, restart ───────────────────────
function Invoke-Upgrade($ver) {
  if ($ver) { $script:Version = $ver }
  Info ("upgrading{0}..." -f $(if ($script:Version) { " to $($script:Version)" } else { '' }))
  Stop-Running
  $script:Reinstall = $true
  Install-Binary
  $script:Reinstall = $false
  Start-IfNeeded
  Ok "upgrade complete (now $(Get-InstalledVersion))"
}

# ── HTTP helpers ─────────────────────────────────────────────────────────────
# -TimeoutSec caps every call so a slow endpoint can't freeze the live redraw.
function Get-Api($path) { Invoke-RestMethod -Uri "$ApiBase$path" -Method Get -TimeoutSec 10 }
function Invoke-ApiPost($path, $obj) {
  if ($null -ne $obj) {
    Invoke-RestMethod -Uri "$ApiBase$path" -Method Post -TimeoutSec 10 `
      -Body ($obj | ConvertTo-Json -Compress) -ContentType 'application/json'
  } else {
    Invoke-RestMethod -Uri "$ApiBase$path" -Method Post -TimeoutSec 10
  }
}
function Remove-Api($path) { Invoke-RestMethod -Uri "$ApiBase$path" -Method Delete -TimeoutSec 10 }

function Read-Prompt($m) { Read-Host -Prompt $m }
function Wait-Key { Read-Host -Prompt '(press enter)' | Out-Null }

# ── Auth (SMS OTP) ───────────────────────────────────────────────────────────
function Invoke-Login {
  Write-Host ''
  Info 'Sign in — Bale SMS one-time-password'
  $phone = Read-Prompt 'Phone number (e.g. 989123456789)'
  if (-not $phone) { Warn 'no phone entered'; return }
  try { $resp = Invoke-ApiPost '/auth/start' @{ phone = $phone } } catch { Warn 'auth/start request failed'; return }
  if (-not $resp.ok) { Warn "auth/start: $($resp.error)"; return }
  Ok "code sent. (registered account: $($resp.isRegistered))"

  $code = Read-Prompt 'Enter the SMS code'
  try {
    $verify = Invoke-ApiPost '/auth/verify' @{
      transactionHash = $resp.transactionHash; code = $code; isRegistered = [bool]$resp.isRegistered
    }
  } catch { Warn 'auth/verify request failed'; return }
  if (-not $verify.ok) { Warn "verify failed: $($verify.error)"; return }
  if ($verify.needsSignup) {
    Info 'new account — choose a display name'
    $name = Read-Prompt 'Display name'
    try { $verify = Invoke-ApiPost '/auth/signup' @{ transactionHash = $verify.transactionHash; name = $name } }
    catch { Warn 'auth/signup request failed'; return }
    if (-not $verify.ok) { Warn "signup failed: $($verify.error)"; return }
  }
  Ok 'signed in.'
}

function Set-Token {
  Write-Host ''
  Info 'Paste a Bale access_token cookie (DevTools -> Application -> Cookies).'
  $t = Read-Prompt 'access_token'
  if (-not $t) { Warn 'empty token'; return }
  try { Invoke-ApiPost '/connect' @{ token = $t } | Out-Null; Ok 'token saved + connecting.' }
  catch { Warn 'connect failed' }
}

# Re-auth chooser: SMS OTP or paste a token (the signed-in menu's option 3).
function Invoke-Reauth {
  Write-Host ''
  Write-Host 'Re-authenticate'
  Write-Host '  1) Sign in with SMS code'
  Write-Host '  2) Paste an access_token cookie'
  Write-Host '  3) Cancel'
  switch (Read-Prompt 'choice>') {
    '1' { Invoke-Login }
    '2' { Set-Token }
    default { }
  }
}

# ── Status from /state ───────────────────────────────────────────────────────
function Show-Status {
  try { $st = Get-Api '/state' } catch { Warn 'could not read /state'; return }
  $mode = if ($st.mode) { $st.mode } else { '-' }
  $signed = if ($st.tokenSet) { 'yes' } else { 'no' }
  if ($st.self -and $st.self.name) { $signed += " ($($st.self.name))" }
  Write-Host ''
  Write-Host '-- Status --------------------------------'
  Write-Host "  binary    : $(Get-InstalledVersion)"
  Write-Host "  signed in : $signed"
  Write-Host "  mode      : $mode"
  Write-Host "  WS ready  : $($st.wsReady)"
  if ($mode -eq 'client') {
    $peer = if ($st.serverPeer -and $st.serverPeer.id) {
      "$(if ($st.serverPeer.name) { $st.serverPeer.name } else { '?' }) [$($st.serverPeer.id)]"
    } else { '(none selected)' }
    Write-Host "  peer      : $peer"
    Write-Host "  tunnel up : $($st.clientRoomReady)"
    if ($st.socks5Port -and $st.socks5Port -ne 0) { Write-Host "  socks5    : 127.0.0.1:$($st.socks5Port)" }
  } elseif ($mode -eq 'server') {
    Write-Host "  clients   : $($st.lkRooms) connected"
  }
  Write-Host '------------------------------------------'
}

function Set-Mode($m) {
  try { Invoke-ApiPost '/config' @{ mode = $m } | Out-Null; Ok "mode -> $m" } catch { Warn 'could not set mode' }
}

# ── Live status banner ───────────────────────────────────────────────────────
# Returns the banner as an array of strings (so callers can CACHE it and avoid
# hitting the API on every keypress). Show-TuiMenu colours lines by prefix.
function Get-StatusBanner {
  $st = $null
  try { $st = Get-Api '/state' } catch { $st = $null }
  $lines = @('')
  if ($null -eq $st) {
    $lines += "  BaleVPN  -  $(Get-InstalledVersion)  -  * API not running"
    $lines += ''
    return ,$lines
  }
  $who = if ($st.tokenSet) { if ($st.self -and $st.self.name) { $st.self.name } else { 'signed in' } } else { 'not signed in' }
  $lines += "  BaleVPN  -  $who  -  $(Get-InstalledVersion)  -  WS:$($st.wsReady)"
  $pend = 0
  $mode = if ($st.mode) { $st.mode } else { '-' }
  switch ($mode) {
    'client' {
      $peer = if ($st.serverPeer -and $st.serverPeer.id) { if ($st.serverPeer.name) { $st.serverPeer.name } else { '?' } } else { 'none' }
      $line = "  mode:client  peer:$peer  up:$($st.clientRoomReady)"
      if ($st.socks5Port -and $st.socks5Port -ne 0) { $line += "  socks:127.0.0.1:$($st.socks5Port)" }
      $lines += $line
    }
    'server' {
      try { $pend = @(Get-Api '/server/pending').Count } catch { $pend = 0 }
      $lines += "  mode:server  clients:$($st.lkRooms)  pending:$pend"
    }
    default { $lines += '  mode:(unset)' }
  }
  # Live alert: new admission requests surface here on their own.
  if ($pend -gt 0) { $lines += "  * $pend pending admission request(s) - open 'Pending requests'" }
  $lines += ''
  return ,$lines
}

# Print cached banner lines, colouring by prefix.
function Write-Banner($lines) {
  foreach ($ln in $lines) {
    if ($ln -match 'not running' -or $ln -match '^\s*\*') { Write-Host $ln -ForegroundColor Yellow }
    elseif ($ln -match '^\s*mode:') { Write-Host $ln -ForegroundColor DarkGray }
    else { Write-Host $ln }
  }
}

# ── Arrow-key menu ───────────────────────────────────────────────────────────
$RefreshSec = 3
$script:TuiIndex = 0

# Show-TuiMenu <statusScriptBlock> <title> <labels[]>
# Up/Down (or k/j) move, Enter selects, digits jump-select, q/Esc/Left go back.
# Re-renders every $RefreshSec seconds while idle so the banner stays live.
function Show-TuiMenu {
  param([scriptblock]$StatusFn, [string]$Title, [string[]]$Labels)
  $sel = 0
  $n = $Labels.Count
  # Cache the banner; refetch only on the timer, never per keystroke.
  $banner = & $StatusFn
  $last = Get-Date
  try { [Console]::CursorVisible = $false } catch {}
  try {
    while ($true) {
      Clear-Host
      Write-Banner $banner
      Write-Host "  $Title" -ForegroundColor White
      for ($i = 0; $i -lt $n; $i++) {
        if ($i -eq $sel) { Write-Host "   > $($Labels[$i])" -ForegroundColor Black -BackgroundColor Gray }
        else             { Write-Host "     $($Labels[$i])" }
      }
      Write-Host '   up/down move - Enter select - Esc back - * live' -ForegroundColor DarkGray
      $key = $null
      $deadline = (Get-Date).AddSeconds($RefreshSec)
      while ((Get-Date) -lt $deadline) {
        if ([Console]::KeyAvailable) { $key = [Console]::ReadKey($true); break }
        Start-Sleep -Milliseconds 80
      }
      if (($null -eq $key) -or (((Get-Date) - $last).TotalSeconds -ge $RefreshSec)) { $banner = & $StatusFn; $last = Get-Date }
      if ($null -eq $key) { continue }     # idle tick -> re-render (refresh)
      switch ($key.Key) {
        'UpArrow'    { $sel = ($sel - 1 + $n) % $n }
        'DownArrow'  { $sel = ($sel + 1) % $n }
        'Enter'      { $script:TuiIndex = $sel; return }
        'Escape'     { $script:TuiIndex = -1; return }
        'LeftArrow'  { $script:TuiIndex = -1; return }
      }
      $ch = "$($key.KeyChar)"
      if ($ch -ceq 'k' -or $ch -ceq 'K') { $sel = ($sel - 1 + $n) % $n }
      elseif ($ch -ceq 'j' -or $ch -ceq 'J') { $sel = ($sel + 1) % $n }
      elseif ($ch -match '^\d$') { $num = [int]$ch; if ($num -ge 1 -and $num -le $n) { $script:TuiIndex = $num - 1; return } }
    }
  } finally { try { [Console]::CursorVisible = $true } catch {} }
}

# Run a leaf action on a cleared screen (cursor shown), then wait for a key.
function LeafBegin { Clear-Host; try { [Console]::CursorVisible = $true } catch {} }
function LeafEnd {
  Write-Host ''
  Write-Host '  (press any key)' -NoNewline
  try { [void][Console]::ReadKey($true) } catch { Read-Host | Out-Null }
}

# ── Client menu ──────────────────────────────────────────────────────────────
function Show-ClientMenu {
  while ($true) {
    Show-TuiMenu { Get-StatusBanner } 'Client' @(
      'List contacts and pick a server peer',
      'Search a contact by phone, then pick',
      'Refresh contacts',
      'Disconnect current peer')
    switch ($script:TuiIndex) {
      0 { LeafBegin; Select-Peer (Get-Api '/peers').peers }
      1 { LeafBegin; $q = Read-Prompt 'phone to search'; Select-Peer (Invoke-ApiPost '/contacts/search' @{ query = $q }).users }
      2 { LeafBegin; Invoke-ApiPost '/peers/refresh' $null | Out-Null; Ok 'refreshed' }
      3 { LeafBegin; Invoke-ApiPost '/tunnel/disconnect' $null | Out-Null; Ok 'disconnected' }
      default { return }
    }
  }
}

function Select-Peer($arr) {
  $arr = @($arr)
  if ($arr.Count -eq 0) { LeafBegin; Warn 'no contacts found'; LeafEnd; return }
  $labels = @($arr | ForEach-Object { "$(if ($_.name) { $_.name } else { '(no name)' })   [$($_.id)]" })
  Show-TuiMenu { Get-StatusBanner } 'Pick a server peer  (Enter = connect)' $labels
  if ($script:TuiIndex -lt 0) { return }
  $peerId = $arr[$script:TuiIndex].id
  LeafBegin
  try { Invoke-ApiPost '/config' @{ serverPeerId = "$peerId" } | Out-Null; Ok "connecting to peer $peerId ..." }
  catch { Warn 'could not set peer' }
  LeafEnd
}

# ── Server menu ──────────────────────────────────────────────────────────────
function Show-ServerMenu {
  while ($true) {
    Show-TuiMenu { Get-StatusBanner } 'Server' @(
      'Pending requests (allow / reject)',
      'Allow-list (auto-accept callers)',
      'Block-list (silently rejected callers)',
      'Max simultaneous clients',
      'Connected clients (view / disconnect)',
      'Connect WS (start accepting calls)',
      'Disconnect WS (stop, drop all clients)')
    switch ($script:TuiIndex) {
      0 { LeafBegin; Show-PendingMenu }
      1 { LeafBegin; Show-CallerList 'Allow-list' '/server/admission' }
      2 { LeafBegin; Show-CallerList 'Block-list' '/server/blacklist' }
      3 { LeafBegin; Set-MaxClients }
      4 { LeafBegin; Show-ConnectedMenu }
      5 { LeafBegin; Invoke-ApiPost '/connect' $null | Out-Null; Ok 'WS connecting' }
      6 { LeafBegin; Invoke-ApiPost '/disconnect' $null | Out-Null; Ok 'WS disconnected' }
      default { return }
    }
  }
}

function Show-PendingMenu {
  while ($true) {
    $rows = @(Get-Api '/server/pending')
    if ($rows.Count -eq 0) { LeafBegin; Info 'no pending requests'; LeafEnd; return }
    $labels = @($rows | ForEach-Object { "$(if ($_.callerName) { $_.callerName } else { '(unknown)' })  [$($_.callerId)]" })
    Show-TuiMenu { Get-StatusBanner } 'Pending requests  (Enter = decide)' $labels
    if ($script:TuiIndex -lt 0) { return }
    $cid = $rows[$script:TuiIndex].callerId
    Show-TuiMenu { Get-StatusBanner } "Caller $cid" @('Allow once', 'Allow always (add to allow-list)', 'Reject (block this caller)')
    switch ($script:TuiIndex) {
      0 { Invoke-ApiPost "/server/pending/$cid/accept" @{ addToList = $false } | Out-Null }
      1 { Invoke-ApiPost "/server/pending/$cid/accept" @{ addToList = $true } | Out-Null }
      2 { Invoke-ApiPost "/server/pending/$cid/reject" $null | Out-Null }
      default { }
    }
  }
}

function Show-CallerList($title, $base) {
  while ($true) {
    $rows = @(Get-Api $base)
    $labels = @('+  Add a caller id') + @($rows | ForEach-Object { "$(if ($_.callerName) { $_.callerName } else { '(unknown)' })  [$($_.callerId)]" })
    Show-TuiMenu { Get-StatusBanner } "$title  (Enter on a caller = remove)" $labels
    if ($script:TuiIndex -lt 0) { return }
    if ($script:TuiIndex -eq 0) {
      LeafBegin
      $cid = Read-Prompt 'caller id to add (Bale numeric uid, blank to cancel)'
      if ($cid) { Invoke-ApiPost $base @{ callerId = "$cid" } | Out-Null; Ok "added $cid" }
      LeafEnd
    } else {
      $cid = $rows[$script:TuiIndex - 1].callerId
      LeafBegin
      Remove-Api "$base/$cid" | Out-Null; Ok "removed $cid"
      LeafEnd
    }
  }
}

function Set-MaxClients {
  $cur = Get-Api '/server/max-clients'
  Write-Host ''
  Info "current cap: $($cur.value)  (max $($cur.max))"
  $val = Read-Prompt 'new value (blank to keep)'
  if (-not $val) { return }
  if ($val -notmatch '^\d+$') { Warn 'not a number'; return }
  try { Invoke-ApiPost '/server/max-clients' @{ value = [int]$val } | Out-Null; Ok "cap -> $val" }
  catch { Warn 'could not set' }
}

function Show-ConnectedMenu {
  while ($true) {
    $rows = @(Get-Api '/tunnel/clients')
    if ($rows.Count -eq 0) { LeafBegin; Info 'no clients connected'; LeafEnd; return }
    $labels = @($rows | ForEach-Object { "$(if ($_.callerName) { $_.callerName } else { '(unknown)' })  [$($_.callerId)]  rx=$($_.rxBytes) tx=$($_.txBytes)" })
    Show-TuiMenu { Get-StatusBanner } 'Connected clients  (Enter = disconnect)' $labels
    if ($script:TuiIndex -lt 0) { return }
    $cid = $rows[$script:TuiIndex].callerId
    LeafBegin
    Invoke-ApiPost "/tunnel/clients/$cid/disconnect" $null | Out-Null; Ok "disconnected $cid"
    LeafEnd
  }
}

# ── Main loop ────────────────────────────────────────────────────────────────
function Show-MainMenu {
  $autoEntered = $false
  while ($true) {
    $st = $null
    try { $st = Get-Api '/state' } catch { $st = $null }

    if ($null -eq $st) {
      # Binary not running (killed or stopped) — offer to (re)start it.
      Show-TuiMenu { Get-StatusBanner } 'Binary not running' @(
        'Start it',
        'Upgrade binary (download + start)',
        'Quit')
      switch ($script:TuiIndex) {
        0 { LeafBegin; Install-Binary; Start-IfNeeded; LeafEnd }
        1 { LeafBegin; Invoke-Upgrade (Read-Prompt 'version (blank = latest)'); LeafEnd }
        default { Clear-Host; exit 0 }
      }
      continue
    }

    if (-not $st.tokenSet) {
      Show-TuiMenu { Get-StatusBanner } 'Not signed in' @(
        'Sign in with SMS code',
        'Paste an access_token cookie',
        'Upgrade binary',
        'Stop the binary',
        'Quit (leave it running)')
      switch ($script:TuiIndex) {
        0 { LeafBegin; Invoke-Login; LeafEnd }
        1 { LeafBegin; Set-Token; LeafEnd }
        2 { LeafBegin; Invoke-Upgrade (Read-Prompt 'version (blank = latest)'); LeafEnd }
        3 { LeafBegin; Stop-Running }
        default { Clear-Host; exit 0 }
      }
      continue
    }

    # On startup, if a mode is already configured, jump straight into it.
    # After backing out (q) we fall through to the main menu to allow switching.
    if (-not $autoEntered) {
      $autoEntered = $true
      if ($st.mode -eq 'client') { Show-ClientMenu; continue }
      elseif ($st.mode -eq 'server') { Show-ServerMenu; continue }
    }

    Show-TuiMenu { Get-StatusBanner } 'Main' @(
      'Client mode (connect to a server)',
      'Server mode (share your connection)',
      'Re-authenticate / paste token',
      'Upgrade binary (stops + restarts it)',
      'Stop the binary',
      'Quit (leave it running)')
    switch ($script:TuiIndex) {
      0 { if ($st.mode -ne 'client') { LeafBegin; Set-Mode 'client' }; Show-ClientMenu }
      1 { if ($st.mode -ne 'server') { LeafBegin; Set-Mode 'server' }; Show-ServerMenu }
      2 { LeafBegin; Invoke-Reauth }
      3 { LeafBegin; Invoke-Upgrade (Read-Prompt 'version (blank = latest)'); LeafEnd }
      4 { LeafBegin; Stop-Running }
      default { Clear-Host; exit 0 }
    }
  }
}

# ── Go ───────────────────────────────────────────────────────────────────────
if ($DoLogs) {
  if (-not (Test-Path $LogFile)) { Die "no log file yet at $LogFile (has the binary been started?)" }
  Info "tailing $LogFile — Ctrl-C to stop"
  Get-Content -Path $LogFile -Tail 200 -Wait
  exit 0
}

if ($DoStop) {
  Stop-Running
  exit 0
} elseif ($DoRestart) {
  Stop-Running
  Install-Binary
  Start-IfNeeded
} elseif ($DoUpgrade) {
  Invoke-Upgrade ''
} elseif ($Reinstall) {
  # A reinstall is only useful if the freshly downloaded binary actually runs,
  # so it stops + re-downloads + restarts (not just overwrite-on-disk).
  Invoke-Upgrade ''
} else {
  Install-Binary
  Start-IfNeeded
}
Show-MainMenu
