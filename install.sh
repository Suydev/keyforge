#!/bin/sh
# KeyForge installer — POSIX sh, no bashisms.
#
# Supports Termux/Android, Linux, and macOS. Windows uses install.ps1.
#
#   curl -fsSL <raw-url>/install.sh | sh
#   ./install.sh --port 9000 --prefix /usr/local
#   ./install.sh --uninstall
#
# Design rules this script follows, because installers that break them are why
# people distrust `curl | sh`:
#
#   * Never install a toolchain silently. Rust is offered, never assumed.
#   * Never write credentials, and never delete the user's key files.
#   * Never edit a shell profile without saying so on stdout.
#   * Every mutation is announced before it happens.
#   * Fail loudly and early rather than half-installing.

set -eu

VERSION="1.0.0"
REPO="https://github.com/Suydev/keyforge"
RAW="https://raw.githubusercontent.com/Suydev/keyforge/main"

PREFIX="${PREFIX:-$HOME/.local}"
SRC_DIR="${SRC_DIR:-$HOME/keyforge}"
PORT="${TABI_PORT:-8787}"
DO_BUILD=1
ALLOW_RUSTUP=1
FORCE=0
UNINSTALL=0

# ── output ───────────────────────────────────────────────────────────────────
# Colour only when stdout is a TTY, so piped output stays clean.
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    C_RESET=$(printf '\033[0m'); C_BOLD=$(printf '\033[1m')
    C_RED=$(printf '\033[31m');  C_GREEN=$(printf '\033[32m')
    C_YELLOW=$(printf '\033[33m'); C_BLUE=$(printf '\033[34m')
    C_DIM=$(printf '\033[2m')
else
    C_RESET=''; C_BOLD=''; C_RED=''; C_GREEN=''; C_YELLOW=''; C_BLUE=''; C_DIM=''
fi

say()  { printf '%s\n' "$*"; }
info() { printf '%s==>%s %s\n' "$C_BLUE$C_BOLD" "$C_RESET" "$*"; }
ok()   { printf '%s  ok%s %s\n' "$C_GREEN" "$C_RESET" "$*"; }
warn() { printf '%swarn%s %s\n' "$C_YELLOW" "$C_RESET" "$*" >&2; }
dim()  { printf '%s     %s%s\n' "$C_DIM" "$*" "$C_RESET"; }
die()  { printf '%sfail%s %s\n' "$C_RED$C_BOLD" "$C_RESET" "$*" >&2; exit 1; }

have() { command -v "$1" >/dev/null 2>&1; }

# Ask a yes/no question. Non-interactive (piped stdin) always answers "no", so
# an unattended run can never be silently upgraded into installing a toolchain.
confirm() {
    if [ ! -t 0 ]; then
        dim "(non-interactive: assuming no)"
        return 1
    fi
    printf '%s [y/N] ' "$1"
    read -r reply || return 1
    case "$reply" in [yY]*) return 0 ;; *) return 1 ;; esac
}

usage() {
    cat <<EOF
${C_BOLD}KeyForge installer v$VERSION${C_RESET}

  sh install.sh [options]

  --prefix DIR    install root                (default $HOME/.local)
  --src DIR       source location             (default $HOME/keyforge)
  --port N        listen port                 (default 8787)
  --no-build      install scripts only
  --no-rust       fail if Rust is missing instead of offering rustup
  --force         overwrite an existing install
  --uninstall     remove binary, launcher and config (key files are kept)
  -h, --help      this text
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix) PREFIX="${2:?--prefix needs a directory}"; shift 2 ;;
        --src)    SRC_DIR="${2:?--src needs a directory}";    shift 2 ;;
        --port)   PORT="${2:?--port needs a number}";         shift 2 ;;
        --no-build) DO_BUILD=0;    shift ;;
        --no-rust)  ALLOW_RUSTUP=0; shift ;;
        --force)    FORCE=1;       shift ;;
        --uninstall) UNINSTALL=1;  shift ;;
        -h|--help)  usage; exit 0 ;;
        *) die "unknown option: $1  (--help for usage)" ;;
    esac
done

case "$PORT" in
    ''|*[!0-9]*) die "--port must be a number, got: $PORT" ;;
esac
[ "$PORT" -ge 1 ] && [ "$PORT" -le 65535 ] || die "--port out of range: $PORT"

BIN_DIR="$PREFIX/bin"
CONFIG_DIR="$HOME/.config/tabi"
LOG_DIR="$HOME/tmp"

# ── platform detection ───────────────────────────────────────────────────────
detect_platform() {
    OS="$(uname -s 2>/dev/null || echo unknown)"
    ARCH="$(uname -m 2>/dev/null || echo unknown)"

    # Termux is Linux, but its paths and package manager differ enough to matter.
    if [ -n "${TERMUX_VERSION:-}" ] || [ -d /data/data/com.termux/files/usr ]; then
        PLATFORM=termux
        SHELL_BIN=/data/data/com.termux/files/usr/bin/bash
        PKG_HINT="pkg install rust clang"
    else
        case "$OS" in
            Linux)  PLATFORM=linux
                    SHELL_BIN=/bin/bash
                    PKG_HINT="apt install build-essential  # or the dnf/pacman equivalent" ;;
            Darwin) PLATFORM=macos
                    SHELL_BIN=/bin/bash
                    PKG_HINT="xcode-select --install" ;;
            MINGW*|MSYS*|CYGWIN*)
                    die "Windows detected. Use install.ps1 instead:
    irm $RAW/install.ps1 | iex" ;;
            *)      die "unsupported OS: $OS" ;;
        esac
    fi

    case "$ARCH" in
        x86_64|amd64|aarch64|arm64) : ;;
        *) warn "untested architecture: $ARCH (continuing anyway)" ;;
    esac
}

# ── uninstall ────────────────────────────────────────────────────────────────
do_uninstall() {
    info "Uninstalling KeyForge"

    if have "$BIN_DIR/tabi"; then
        "$BIN_DIR/tabi" stop 2>/dev/null || true
    fi

    for f in "$BIN_DIR/keyforge" "$BIN_DIR/tabi"; do
        if [ -e "$f" ]; then rm -f "$f" && ok "removed $f"; fi
    done

    if [ -d "$CONFIG_DIR" ]; then
        rm -rf "$CONFIG_DIR" && ok "removed $CONFIG_DIR"
    fi

    say ""
    say "Left in place, deliberately:"
    dim "$SRC_DIR             (source tree)"
    dim "your *-keys.txt      (may be your only copy)"
    dim "your proxies.txt"
    say ""
    ok "done"
    exit 0
}

# ── toolchain ────────────────────────────────────────────────────────────────
ensure_rust() {
    if have cargo; then
        ok "cargo $(cargo --version 2>/dev/null | cut -d' ' -f2)"
        return 0
    fi

    # rustup installs to ~/.cargo/bin, which may simply not be on PATH yet.
    if [ -x "$HOME/.cargo/bin/cargo" ]; then
        PATH="$HOME/.cargo/bin:$PATH"; export PATH
        ok "found cargo in ~/.cargo/bin (added to PATH for this run)"
        return 0
    fi

    warn "Rust is not installed."
    if [ "$ALLOW_RUSTUP" -eq 0 ]; then
        die "--no-rust was given. Install Rust and re-run:  $PKG_HINT"
    fi

    if [ "$PLATFORM" = termux ]; then
        say "  On Termux, the packaged toolchain is the reliable route:"
        say "      pkg install rust"
        confirm "  Run that now?" || die "Rust required. Install it and re-run."
        pkg install -y rust || die "pkg install rust failed"
    else
        say "  Official installer: https://rustup.rs"
        confirm "  Download and run rustup now?" \
            || die "Rust required. Install it and re-run."
        have curl || die "curl is needed to fetch rustup"
        curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs \
            | sh -s -- -y --no-modify-path \
            || die "rustup failed"
        PATH="$HOME/.cargo/bin:$PATH"; export PATH
    fi

    have cargo || die "cargo still not on PATH after install"
    ok "cargo ready"
}

ensure_linker() {
    for c in cc gcc clang; do
        if have "$c"; then ok "linker: $c"; return 0; fi
    done
    warn "No C linker found. The 'ring' crypto crate needs one."
    say  "  Install with:  $PKG_HINT"
    confirm "  Continue anyway?" || die "aborted"
}

# ── source ───────────────────────────────────────────────────────────────────
ensure_source() {
    # Running from a checkout: use it in place rather than cloning over the net.
    if [ -f "Cargo.toml" ] && [ -d "src" ] && grep -q '^name *= *"keyforge"' Cargo.toml 2>/dev/null; then
        SRC_DIR="$(pwd)"
        ok "building from the current directory: $SRC_DIR"
        return 0
    fi

    if [ -d "$SRC_DIR/src" ] && [ -f "$SRC_DIR/Cargo.toml" ]; then
        if [ "$FORCE" -eq 1 ]; then
            info "Updating existing source at $SRC_DIR"
            if [ -d "$SRC_DIR/.git" ] && have git; then
                ( cd "$SRC_DIR" && git pull --ff-only ) || warn "git pull failed; building what is there"
            fi
        else
            ok "using existing source at $SRC_DIR"
            dim "(--force to git pull first)"
        fi
        return 0
    fi

    have git || die "git is needed to fetch the source (or run this from a checkout)"
    info "Cloning $REPO"
    git clone --depth 1 "$REPO" "$SRC_DIR" || die "clone failed"
    ok "cloned to $SRC_DIR"
}

# ── build ────────────────────────────────────────────────────────────────────
build() {
    [ "$DO_BUILD" -eq 1 ] || { warn "--no-build: skipping cargo"; return 0; }

    info "Building (release). Several minutes on low-power hardware."

    # A release build pins every core. On a battery-powered device that is the
    # difference between finishing and dying mid-link.
    if have termux-battery-status && have python3; then
        _bat="$(termux-battery-status 2>/dev/null | python3 -c \
            'import json,sys
try:
    d=json.load(sys.stdin); print(d["level"], d["status"])
except Exception: print("")' 2>/dev/null || true)"
        set -- $_bat
        if [ "${1:-100}" -lt 25 ] 2>/dev/null && [ "${2:-CHARGING}" != "CHARGING" ]; then
            warn "battery ${1}% and not charging — a release build may not finish"
            confirm "  Continue?" || die "aborted; plug in and re-run"
        fi
    fi

    ( cd "$SRC_DIR" && cargo build --release ) || die "build failed
  Out of memory?  cd $SRC_DIR && cargo build --release -j1"

    [ -f "$SRC_DIR/target/release/keyforge" ] \
        || die "build reported success but the binary is missing"
    ok "built $(du -h "$SRC_DIR/target/release/keyforge" | cut -f1)"
}

install_binary() {
    [ "$DO_BUILD" -eq 1 ] || return 0
    mkdir -p "$BIN_DIR"
    install -m 755 "$SRC_DIR/target/release/keyforge" "$BIN_DIR/keyforge" \
        2>/dev/null \
        || { cp "$SRC_DIR/target/release/keyforge" "$BIN_DIR/keyforge" \
             && chmod 755 "$BIN_DIR/keyforge"; } \
        || die "could not install to $BIN_DIR"
    ok "installed $BIN_DIR/keyforge"
}

# ── launcher ─────────────────────────────────────────────────────────────────
# Generated rather than shipped: the shebang and the paths differ per platform,
# and a hardcoded Termux shebang is exactly the kind of thing that makes a
# "cross-platform" project fail on first contact with a laptop.
write_launcher() {
    info "Generating the tabi launcher"
    mkdir -p "$BIN_DIR" "$LOG_DIR"
    target="$BIN_DIR/tabi"

    cat > "$target" <<LAUNCHER
#!$SHELL_BIN
# tabi — start / stop / inspect the KeyForge.
# GENERATED by install.sh for platform: $PLATFORM. Re-run the installer to regenerate.
#
#   tabi            start in the background (no-op if already up)
#   tabi stop|restart|status|log|open|fg

set -uo pipefail

BIN="$BIN_DIR/keyforge"
LOG="$LOG_DIR/keyforge.log"
PORT="\${TABI_PORT:-$PORT}"

mkdir -p "\$(dirname "\$LOG")" 2>/dev/null

# bash /dev/tcp so no curl or nc is required for the liveness check.
up() { (exec 3<>"/dev/tcp/127.0.0.1/\$PORT") 2>/dev/null; }

need_bin() {
  if [ ! -x "\$BIN" ]; then
    echo "tabi: binary not found at \$BIN" >&2
    echo "  build it:  cd $SRC_DIR && cargo build --release" >&2
    exit 1
  fi
}

start() {
  need_bin
  if up; then echo "tabi: already running on 127.0.0.1:\$PORT"; return 0; fi
  # setsid + nohup so it outlives this shell, the terminal, and whatever agent
  # launched it. Without setsid, closing the terminal takes the gateway with it.
  TABI_PORT="\$PORT" setsid nohup "\$BIN" >>"\$LOG" 2>&1 </dev/null &
  disown 2>/dev/null || true
  for _ in \$(seq 1 20); do
    sleep 0.3
    if up; then
      echo "tabi: up on http://127.0.0.1:\$PORT  (dashboard: http://127.0.0.1:\$PORT/)"
      return 0
    fi
  done
  echo "tabi: failed to start within 6s — last log lines:" >&2
  tail -n 12 "\$LOG" >&2
  return 1
}

stop() {
  if ! pgrep -f "release/keyforge|bin/keyforge" >/dev/null 2>&1; then
    echo "tabi: not running"; return 0
  fi
  # SIGTERM first: state is flushed on term, so prefer it to SIGKILL.
  pkill -f "release/keyforge|bin/keyforge" 2>/dev/null
  for _ in \$(seq 1 10); do sleep 0.3; up || break; done
  if up; then
    pkill -9 -f "release/keyforge|bin/keyforge" 2>/dev/null
    sleep 1
  fi
  echo "tabi: stopped"
}

status() {
  if ! up; then echo "tabi: DOWN (port \$PORT not listening)"; return 1; fi
  if ! command -v python3 >/dev/null 2>&1 || ! command -v curl >/dev/null 2>&1; then
    echo "tabi: UP on 127.0.0.1:\$PORT  (install curl+python3 for the full summary)"
    return 0
  fi
  # Do NOT pipe curl into \`python3 - <<'PY'\`: the heredoc and the pipe both claim
  # fd 0, so python reads the script and the body is lost. Fetch to a file first.
  snap="\$(mktemp "\${TMPDIR:-\$HOME/tmp}/tabi-snap.XXXXXX")" || return 1
  if ! curl -s -m 6 "http://127.0.0.1:\$PORT/api/snapshot" -o "\$snap" 2>/dev/null; then
    rm -f "\$snap"; echo "tabi: up, but /api/snapshot did not respond"; return 1
  fi
  SNAP="\$snap" python3 - <<'PY'
import json, os, sys
try:
    with open(os.environ["SNAP"]) as fh:
        d = json.load(fh)
except Exception as e:
    print("tabi: up, but /api/snapshot could not be parsed: {}".format(e)); sys.exit(0)
t = d["totals"]
print("tabi: UP  offline={}  requests={} errors={} spend=\${:.4f}  {} active session(s)".format(
    d["offline"], t["requests"], t["errors"], t["cost"], d["activeSessions"]))
print("      saves: {} key rotations, {} failovers, {} offline holds".format(
    t["rotations"], t["failovers"], t["offlineHolds"]))
print("      data:  {:.1f} MB through the gateway".format(t["bytesTotal"] / 1048576))
for p in d["providers"]:
    ew = "{}ms".format(int(p["ewmaMs"])) if p["ewmaMs"] else "unmeasured"
    print("  {:<10} {:>4}/{:<4} keys  \${:>11,.2f}  {:>10}  up {}%  req={} err={}".format(
        p["id"], p["alive"], p["keys"], p["funds"], ew, p["uptimePct"],
        p["requests"], p["errors"]))
PY
  rm -f "\$snap"
}

case "\${1:-start}" in
  start|"") start ;;
  stop)     stop ;;
  restart)  stop; sleep 1; start ;;
  status)   status ;;
  log)      tail -n 40 -f "\$LOG" ;;
  open)     echo "http://127.0.0.1:\$PORT/" ;;
  fg)       need_bin; exec env TABI_PORT="\$PORT" "\$BIN" ;;
  *) echo "usage: tabi {start|stop|restart|status|log|open|fg}" >&2; exit 2 ;;
esac
LAUNCHER

    chmod 755 "$target"
    ok "wrote $target"
}

# ── config ───────────────────────────────────────────────────────────────────
write_config() {
    mkdir -p "$CONFIG_DIR"
    cfg="$CONFIG_DIR/providers.json"

    if [ -f "$cfg" ] && [ "$FORCE" -eq 0 ]; then
        ok "keeping existing $cfg"
        return 0
    fi

    info "Writing starter config"
    cat > "$cfg" <<'JSON'
{
  "version": 1,
  "providers": [
    {
      "id": "provider-a",
      "label": "Provider A",
      "hosts": [{ "host": "api.example.com", "enabled": true, "note": "primary" }],
      "keys_file": "keys/provider-a-keys.txt",
      "hold": 0.80,
      "initial_guess": 120.0,
      "enabled": true,
      "bias": 0.0,
      "note": "edit host and keys_file, then restart"
    }
  ],
  "routing": {
    "slow_multiplier": 2.0,
    "min_samples": 3,
    "error_weight": 4.0,
    "streak_weight": 0.5,
    "streak_halflife_secs": 120,
    "breaker_trip": 3,
    "breaker_backoff_secs": [15, 45, 120],
    "missing_model_penalty": 50.0,
    "probe_heals_score": true,
    "session_stickiness": true,
    "sticky_escape_multiplier": 4.0
  }
}
JSON
    ok "wrote $cfg"
    dim "placeholder host — edit before starting"
}

# ── PATH ─────────────────────────────────────────────────────────────────────
ensure_path() {
    case ":$PATH:" in
        *":$BIN_DIR:"*) ok "$BIN_DIR already on PATH"; return 0 ;;
    esac

    line="export PATH=\"$BIN_DIR:\$PATH\""
    for rc in "$HOME/.bashrc" "$HOME/.zshrc" "$HOME/.profile"; do
        [ -f "$rc" ] || continue
        if grep -Fq "$BIN_DIR" "$rc" 2>/dev/null; then
            ok "PATH entry already present in $rc"
            return 0
        fi
        printf '\n# added by keyforge installer\n%s\n' "$line" >> "$rc"
        ok "added $BIN_DIR to PATH in $rc"
        dim "run:  . $rc    (or open a new shell)"
        return 0
    done

    warn "no shell profile found; add this yourself:"
    say  "    $line"
}

# ── main ─────────────────────────────────────────────────────────────────────
main() {
    say ""
    say "${C_BOLD}KeyForge${C_RESET} installer v$VERSION"
    say ""

    detect_platform
    ok "platform: $PLATFORM ($OS/$ARCH)"

    [ "$UNINSTALL" -eq 1 ] && do_uninstall

    if [ -x "$BIN_DIR/keyforge" ] && [ "$FORCE" -eq 0 ]; then
        warn "already installed at $BIN_DIR/keyforge"
        confirm "  Reinstall?" || { say "nothing to do"; exit 0; }
    fi

    if [ "$DO_BUILD" -eq 1 ]; then
        ensure_rust
        ensure_linker
    fi
    ensure_source
    build
    install_binary
    write_launcher
    write_config
    ensure_path

    say ""
    say "${C_GREEN}${C_BOLD}Installed.${C_RESET}"
    say ""
    say "${C_BOLD}Next:${C_RESET}"
    say "  1. Add your API keys — one per line:"
    dim "     mkdir -p ~/keys && \$EDITOR ~/keys/provider-a-keys.txt"
    say "  2. Point the config at your provider:"
    dim "     \$EDITOR $CONFIG_DIR/providers.json"
    say "  3. Start it:"
    dim "     tabi start        # then: tabi status"
    say ""
    say "${C_BOLD}Endpoints${C_RESET} once running:"
    dim "dashboard  http://127.0.0.1:$PORT/"
    dim "anthropic  http://127.0.0.1:$PORT/v1/messages"
    dim "openai     http://127.0.0.1:$PORT/v1/chat/completions"
    say ""
    say "Optional: ~/proxies.txt for egress rotation — see docs/CONFIGURATION.md"
    if [ "$PLATFORM" = termux ]; then
        say "Optional: Shizuku/rish for Wi-Fi link sampling — see docs/INSTALL.md"
    fi
    say ""
}

main
