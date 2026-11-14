#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-or-later
#
# install.sh — build & install Mochi Mochi as a bare-metal Wayland session.
#
# Installs (default --prefix /usr/local):
#   <prefix>/bin/mochi-compositor                     compositor binary
#   <prefix>/bin/mochi-msg                            IPC client
#   <prefix>/bin/mochi-session                        login-manager launcher
#   <prefix>/share/mochi-mochi/mochi.kdl              default config (seeded to ~/.config/mochi/)
#   <prefix>/lib/systemd/user/mochi-mochi-session.target
#   <prefix>/share/xdg-desktop-portal/mochimochi-portals.conf
#   /usr/share/wayland-sessions/mochi-mochi.desktop   login manager entry (GDM / SDDM / greetd)
#
# Usage: ./install.sh [OPTIONS]        (run as your normal user; it asks for sudo when needed)
#   --prefix DIR            Installation prefix                    (default: /usr/local)
#   --destdir DIR           Stage into DIR (packaging; no root, no sudo)
#   --sessions-dir DIR      Wayland session entry directory        (default: /usr/share/wayland-sessions)
#   --systemd-user-dir DIR  systemd user unit directory            (default: <prefix>/lib/systemd/user)
#   --no-build              Skip `cargo build --release`
#   --uninstall             Remove a previous installation (use the same options)
#   --dry-run               Print actions without changing anything
#   -h, --help              Show this help
#
# Environment: CARGO (cargo binary), CARGO_FLAGS (extra flags, e.g. "--offline").

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

PREFIX="/usr/local"
DESTDIR=""
SESSIONS_DIR="/usr/share/wayland-sessions"
SYSTEMD_USER_DIR=""
DO_BUILD=1
UNINSTALL=0
DRY_RUN=0
ORIG_ARGS=("$@")

usage() { sed -n '4,25p' "$0" | sed 's/^# \{0,1\}//'; }
die()   { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }
info()  { printf '\033[1;35m==>\033[0m %s\n' "$*"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --prefix)            PREFIX="${2:?--prefix needs a value}"; shift 2 ;;
        --prefix=*)          PREFIX="${1#*=}"; shift ;;
        --destdir)           DESTDIR="${2:?--destdir needs a value}"; shift 2 ;;
        --destdir=*)         DESTDIR="${1#*=}"; shift ;;
        --sessions-dir)      SESSIONS_DIR="${2:?--sessions-dir needs a value}"; shift 2 ;;
        --sessions-dir=*)    SESSIONS_DIR="${1#*=}"; shift ;;
        --systemd-user-dir)  SYSTEMD_USER_DIR="${2:?--systemd-user-dir needs a value}"; shift 2 ;;
        --systemd-user-dir=*) SYSTEMD_USER_DIR="${1#*=}"; shift ;;
        --no-build)          DO_BUILD=0; shift ;;
        --uninstall)         UNINSTALL=1; DO_BUILD=0; shift ;;
        --dry-run)           DRY_RUN=1; shift ;;
        -h|--help)           usage; exit 0 ;;
        *)                   die "unknown option: $1 (see --help)" ;;
    esac
done

PREFIX="${PREFIX%/}"
BINDIR="$PREFIX/bin"
DATADIR="$PREFIX/share"
SYSTEMD_USER_DIR="${SYSTEMD_USER_DIR:-$PREFIX/lib/systemd/user}"
RELEASE_DIR="$SCRIPT_DIR/target/release"

FILES_BIN=(mochi-compositor mochi-msg mochi-session)
DEST_CONFIG="$DATADIR/mochi-mochi/mochi.kdl"
DEST_PROTOCOL="$DATADIR/mochi-mochi/protocols/mochi-glass-v1.xml"
DEST_DESKTOP="$SESSIONS_DIR/mochi-mochi.desktop"
DEST_TARGET="$SYSTEMD_USER_DIR/mochi-mochi-session.target"
DEST_PORTAL="$DATADIR/xdg-desktop-portal/mochimochi-portals.conf"

run() {
    if [ "$DRY_RUN" -eq 1 ]; then printf '    %s\n' "$*"; else "$@"; fi
}

# ---------------------------------------------------------------------------
# 1. Build (always as the invoking, unprivileged user)
# ---------------------------------------------------------------------------
find_cargo() {
    if [ -n "${CARGO:-}" ]; then echo "$CARGO"; return; fi
    if command -v cargo >/dev/null 2>&1; then command -v cargo; return; fi
    local home="$HOME"
    [ -n "${SUDO_USER:-}" ] && home="$(getent passwd "$SUDO_USER" | cut -d: -f6)"
    [ -x "$home/.cargo/bin/cargo" ] && { echo "$home/.cargo/bin/cargo"; return; }
    die "cargo not found; install Rust (https://rustup.rs) or set CARGO=/path/to/cargo"
}

if [ "$DO_BUILD" -eq 1 ]; then
    CARGO_BIN="$(find_cargo)"
    # shellcheck disable=SC2206
    build_cmd=("$CARGO_BIN" build --release --locked -p mochi-compositor -p mochi-ipc ${CARGO_FLAGS:-})
    info "Building release binaries: ${build_cmd[*]}"
    if [ "$(id -u)" -eq 0 ] && [ -n "${SUDO_USER:-}" ]; then
        # Never leave root-owned files in target/
        run sudo -u "$SUDO_USER" -H "${build_cmd[@]}"
    else
        run "${build_cmd[@]}"
    fi
fi

# ---------------------------------------------------------------------------
# 2. Acquire privileges for system directories (unless staging into --destdir)
# ---------------------------------------------------------------------------
needs_root=0
if [ -z "$DESTDIR" ] && [ "$(id -u)" -ne 0 ]; then
    for dir in "$BINDIR" "$DATADIR" "$SESSIONS_DIR" "$SYSTEMD_USER_DIR"; do
        probe="$dir"
        while [ ! -d "$probe" ]; do probe="$(dirname "$probe")"; done
        [ -w "$probe" ] || needs_root=1
    done
fi
if [ "$needs_root" -eq 1 ] && [ "$DRY_RUN" -eq 0 ]; then
    command -v sudo >/dev/null 2>&1 || die "root privileges required (re-run as root)"
    info "Elevating with sudo to write system directories"
    extra=()
    [ "$UNINSTALL" -eq 0 ] && extra+=(--no-build)
    exec sudo "$0" ${ORIG_ARGS[@]+"${ORIG_ARGS[@]}"} ${extra[@]+"${extra[@]}"}
fi

# ---------------------------------------------------------------------------
# 3. Uninstall
# ---------------------------------------------------------------------------
if [ "$UNINSTALL" -eq 1 ]; then
    info "Uninstalling Mochi Mochi from ${DESTDIR}${PREFIX}"
    for bin in "${FILES_BIN[@]}"; do run rm -f "$DESTDIR$BINDIR/$bin"; done
    run rm -f "$DESTDIR$DEST_CONFIG" "$DESTDIR$DEST_PROTOCOL" "$DESTDIR$DEST_DESKTOP" "$DESTDIR$DEST_TARGET" "$DESTDIR$DEST_PORTAL"
    run rmdir --ignore-fail-on-non-empty "$DESTDIR$DATADIR/mochi-mochi/protocols" 2>/dev/null || true
    run rmdir --ignore-fail-on-non-empty "$DESTDIR$DATADIR/mochi-mochi" 2>/dev/null || true
    info "Done. Per-user files (~/.config/mochi, ~/.local/state/mochi) were left untouched."
    exit 0
fi

# ---------------------------------------------------------------------------
# 4. Install
# ---------------------------------------------------------------------------
for bin in mochi-compositor mochi-msg; do
    [ -x "$RELEASE_DIR/$bin" ] || [ "$DRY_RUN" -eq 1 ] || \
        die "missing $RELEASE_DIR/$bin — build first (make) or drop --no-build"
done

info "Installing Mochi Mochi into ${DESTDIR}${PREFIX}"
run install -Dm755 "$RELEASE_DIR/mochi-compositor" "$DESTDIR$BINDIR/mochi-compositor"
run install -Dm755 "$RELEASE_DIR/mochi-msg"        "$DESTDIR$BINDIR/mochi-msg"

# Session launcher with absolute paths baked in (login managers have a minimal PATH)
tmp="$(mktemp)"; trap 'rm -f "$tmp"' EXIT
sed -e "s|@BINDIR@|$BINDIR|g" -e "s|@DATADIR@|$DATADIR|g" resources/mochi-session > "$tmp"
run install -Dm755 "$tmp" "$DESTDIR$BINDIR/mochi-session"

sed -e "s|^Exec=mochi-session|Exec=$BINDIR/mochi-session|" \
    -e "s|^TryExec=mochi-session|TryExec=$BINDIR/mochi-session|" \
    resources/mochi-mochi.desktop > "$tmp"
run install -Dm644 "$tmp" "$DESTDIR$DEST_DESKTOP"

run install -Dm644 config/mochi.kdl                               "$DESTDIR$DEST_CONFIG"
run install -Dm644 protocols/mochi-glass-v1.xml                   "$DESTDIR$DEST_PROTOCOL"
run install -Dm644 resources/systemd/mochi-mochi-session.target   "$DESTDIR$DEST_TARGET"
run install -Dm644 resources/mochimochi-portals.conf              "$DESTDIR$DEST_PORTAL"

cat <<EOF

$(info "Mochi Mochi installed")
    binaries      $BINDIR/{mochi-compositor,mochi-msg,mochi-session}
    session entry $DEST_DESKTOP
    default conf  $DEST_CONFIG  (copied to ~/.config/mochi/mochi.kdl on first login)
    systemd unit  $DEST_TARGET
    logs          ~/.local/state/mochi/mochi-compositor.log

  Log out and pick "Mochi Mochi" in GDM / SDDM, or for greetd set:
      command = "$BINDIR/mochi-session"
EOF
