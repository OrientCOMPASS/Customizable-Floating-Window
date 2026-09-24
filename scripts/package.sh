#!/usr/bin/env bash
# Assemble a cross-platform plugin zip from locally built artifacts.
#
#   scripts/package.sh            # linux native + windows cross (mingw) if present
#
# The FULL three-platform bundle (incl. macOS dylibs) is produced by
# .github/workflows/release.yml — macOS binaries require the Apple SDK and
# cannot be cross-compiled from Linux. Zips built here are therefore labeled
# `-linux+windows` and are intended for development / LAN distribution.
set -euo pipefail
cd "$(dirname "$0")/.."

PROFILE="${PROFILE:-release}"
OUT="dist/stage"
rm -rf "$OUT" dist/plugin-linux+windows.zip
mkdir -p "$OUT/bin" "$OUT/themes"

cp plugin.json panel.html LICENSE README.md "$OUT/"
cp themes/*.slint "$OUT/themes/"

# Linux (native)
if [ -f "target/$PROFILE/libcustomizable_floating_window.so" ]; then
  cp "target/$PROFILE/libcustomizable_floating_window.so" "$OUT/customizable_floating_window.so"
  # (no helper binary for Linux: the Slint UI runs in-process since round 5)
fi

# Windows (cross via mingw, or native on Windows runners).
# NOTE: no helper binary for Windows/Linux — since round 5 the Slint UI runs
# in-process on a plugin-owned thread there; only macOS ships the helper
# subprocess (winit main-thread rule).
for cand in \
  "target/x86_64-pc-windows-gnu/$PROFILE/customizable_floating_window.dll" \
  "target/$PROFILE/customizable_floating_window.dll"; do
  if [ -f "$cand" ]; then
    cp "$cand" "$OUT/customizable_floating_window.dll"
    break
  fi
done

# macOS helper (built natively on a Mac; absent in local dev bundles)
if [ -f "target/$PROFILE/floating_helper" ] && [ "$(uname)" = "Darwin" ]; then
  cp "target/$PROFILE/floating_helper" "$OUT/bin/floating-helper-macos-aarch64"
  chmod +x "$OUT/bin/floating-helper-macos-aarch64"
fi

# bin/ note: helper subprocess exists only for macOS (winit main-thread rule)
mkdir -p "$OUT/bin"
cat > "$OUT/bin/README.txt" <<'EOF'
bin/ holds the floating_helper subprocess binary for macOS only
(floating-helper-macos-aarch64): on macOS, winit/Slint require the process
main thread for the event loop, which the Tauri host owns, so the UI runs in
this helper. On Windows/Linux the Slint UI runs inside the plugin process on
a plugin-owned thread — no helper binary is needed or shipped.
The official release zip (GitHub Release) contains the macOS helper; local
dev bundles built off-macOS omit it.
EOF

( cd "$OUT" && zip -qr "../plugin-linux+windows.zip" . )
echo "==> dist/plugin-linux+windows.zip"
python3 - <<'PY'
import zipfile
for i in zipfile.ZipFile("dist/plugin-linux+windows.zip").infolist():
    print(f"{i.file_size:>9}  {i.filename}")
PY
