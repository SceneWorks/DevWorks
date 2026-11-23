#!/usr/bin/env bash
set -euo pipefail

mode=$1
root=$2
target=$3
receipt="$root/qualification.json"
archive="$root/devworks-native-pdf-$target.tar.gz"
archive_hash="$(jq -er --arg target "$target" 'select(.target == $target) | .archiveSha256' "$receipt")"
printf '%s  %s\n' "$archive_hash" "$archive" | sha256sum --check --strict

verify_members() {
  local install=$1
  printf '%s  %s\n' "$(jq -er .desktop.sha256 "$receipt")" "$install/devworks-desktop" \
    "$(jq -er .worker.sha256 "$receipt")" "$install/devworks-pdf-worker" | sha256sum --check --strict
}

unpack() {
  local destination=$1
  mkdir -p "$destination"
  tar -xzf "$archive" --no-same-owner -C "$destination"
  verify_members "$destination"
}

arch=$(dpkg --print-architecture)
case "$target/$arch" in
  x86_64-unknown-linux-gnu/amd64 | aarch64-unknown-linux-gnu/arm64) ;;
  *) printf 'Wrong native package target %s/%s\n' "$target" "$arch" >&2; exit 1 ;;
esac

package="$root/devworks-qualification.deb"
install=/usr/lib/devworks-qualification
if [ "$mode" = stage ]; then
  stage="$root/stage"
  unpack "$stage$install"
  mkdir -p "$stage/DEBIAN" "$stage/usr/bin" "$stage/usr/share/applications" "$root/shlib/debian"
  ln -s ../lib/devworks-qualification/devworks-desktop "$stage/usr/bin/devworks-qualification"
  cat > "$stage/usr/share/applications/devworks-qualification.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=DevWorks Qualification
Exec=devworks-qualification
Terminal=false
Categories=Development;
DESKTOP
  sudo apt-get update
  sudo apt-get install -y dpkg-dev
  cat > "$root/shlib/debian/control" <<'CONTROL'
Source: devworks-qualification
Maintainer: DevWorks qualification <devworks@example.invalid>

Package: devworks-qualification
Architecture: any
Description: DevWorks Linux qualification build
CONTROL
  dependencies=$(
    cd "$root/shlib"
    dpkg-shlibdeps -O -e"$stage$install/devworks-desktop" -e"$stage$install/devworks-pdf-worker" |
      sed -n 's/^shlibs:Depends=//p'
  )
  test -n "$dependencies"
  cat > "$stage/DEBIAN/control" <<CONTROL
Package: devworks-qualification
Version: 0.1.0~qualification
Architecture: $arch
Maintainer: DevWorks qualification <devworks@example.invalid>
Depends: $dependencies, libegl1, libgl1, libgl1-mesa-dri, libwayland-client0, libxkbcommon0, libx11-6, libxcursor1, libxi6, libxrandr2
Description: DevWorks Linux qualification build
CONTROL
  dpkg-deb --root-owner-group --build "$stage" "$package"
  dpkg-deb --info "$package"
  exit 0
fi
test "$mode" = verify

cat /etc/os-release
uname -a
test "$(dpkg-deb -f "$package" Architecture)" = "$arch"
sudo apt-get update
sudo apt-get install -y "$package" xvfb dbus-x11 weston xauth libgl1-mesa-dri
verify_members "$install"
ldd "$install/devworks-desktop"
ldd "$install/devworks-pdf-worker"
"$install/devworks-pdf-worker" --check-pdf-worker-memory

relocated="$root/DevWorks relocated ü"
unpack "$relocated"
"$relocated/devworks-pdf-worker" --check-pdf-worker-memory
results="$root/results"
mkdir -p "$results"
runtime="$root/runtime"
mkdir -m 700 "$runtime" "$root/no-tools"

export XDG_RUNTIME_DIR="$runtime" LIBGL_ALWAYS_SOFTWARE=1
timeout 75 dbus-run-session -- xvfb-run -a env -u WAYLAND_DISPLAY XDG_SESSION_TYPE=x11 PATH="$root/no-tools" \
  /usr/bin/devworks-qualification --data-dir "$root/profile-x11" \
  --screenshot "$results/x11.png" --close-after-secs 12 > "$results/x11.log" 2>&1

export WAYLAND_DISPLAY=wayland-1
unset DISPLAY
weston --backend=headless-backend.so --socket="$WAYLAND_DISPLAY" --idle-time=0 --log="$results/weston.log" &
compositor=$!
trap 'kill "$compositor" 2>/dev/null || true' EXIT
for ((attempt = 0; attempt < 50; attempt++)); do
  test -S "$runtime/$WAYLAND_DISPLAY" && break
  sleep 0.1
done
test -S "$runtime/$WAYLAND_DISPLAY"
timeout 75 dbus-run-session -- env XDG_SESSION_TYPE=wayland PATH="$root/no-tools" "$relocated/devworks-desktop" \
  --data-dir "$root/profile-wayland" --screenshot "$results/wayland.png" \
  --close-after-secs 12 > "$results/wayland.log" 2>&1

python3 -I - "$root" <<'PY'
from pathlib import Path
import sqlite3
import sys

root = Path(sys.argv[1])
for display in ("x11", "wayland"):
    path = root / f"profile-{display}" / "workspace.sqlite"
    with sqlite3.connect(path.as_uri() + "?mode=ro", uri=True) as database:
        clean = database.execute("select count(*) from app_boots where shutdown_state = 'Clean'").fetchone()[0]
        assert clean == 1, (display, clean)
    frame = (root / "results" / f"{display}.png").read_bytes()
    assert frame[:8] == b"\x89PNG\r\n\x1a\n" and frame.endswith(b"IEND\xaeB`\x82") and len(frame) > 4096, display
    print(f"{display}: exact candidate rendered a frame and shut down cleanly")
PY

sudo apt-get remove -y devworks-qualification
test ! -e "$install/devworks-desktop"
test ! -e /usr/bin/devworks-qualification
