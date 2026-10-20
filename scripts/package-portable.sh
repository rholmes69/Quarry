#!/usr/bin/env bash
# Build a portable Quarry bundle for Linux or macOS.
#
#   scripts/package-portable.sh                 # native target
#   GLIBC=2.28 scripts/package-portable.sh      # Linux: run on older distros
#                                               # (needs `cargo install cargo-zigbuild` + zig)
#
# Output: dist/Quarry-<version>-<os>-<arch>-portable.tar.gz containing
#   quarry                 the single executable (SQLite3MC, GUI, drivers, TLS)
#   QuarryData/            presence of this folder switches on portable mode
#   README-PORTABLE.txt
set -euo pipefail
cd "$(dirname "$0")/.."
version=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
os=$(uname -s | tr '[:upper:]' '[:lower:]'); arch=$(uname -m)

if [[ -n "${GLIBC:-}" && "$os" == "linux" ]]; then
  target="${arch}-unknown-linux-gnu"
  cargo zigbuild --release --target "${target}.${GLIBC}"
  bin="target/${target}/release/quarry"
  suffix="-glibc${GLIBC}"
else
  cargo build --release
  bin="target/release/quarry"
  suffix=""
fi

name="Quarry-${version}-${os}-${arch}${suffix}-portable"
out="dist/${name}"
rm -rf "$out" && mkdir -p "$out/QuarryData"
cp "$bin" "$out/quarry"
cat > "$out/README-PORTABLE.txt" <<TXT
Quarry ${version} — portable edition

Run ./quarry (optionally: ./quarry path/to/database.db).
Nothing to install. Settings, recent files, connection profiles and custom
collations are kept in the QuarryData folder next to the program; delete that
folder to switch Quarry back to the per-user settings location.
Passwords are never stored.
TXT
tar -C dist -czf "dist/${name}.tar.gz" "$name"
echo "built dist/${name}.tar.gz"
echo "runtime libraries required:"
(ldd "$out/quarry" 2>/dev/null || otool -L "$out/quarry") | sed 's/^/  /'
