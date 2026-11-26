#!/usr/bin/env bash
# 打一个最小可用的 .deb。
#
# 不追求覆盖所有发行版，只为让"下载后能装上、能在应用菜单里找到"这条路跑通。
# AppImage 需要 linuxdeploy / appimagetool，本机没有，等真正发布时再加。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VERSION="${VERSION:-$(grep -m1 '^version' "$ROOT/Cargo.toml" | sed 's/.*"\(.*\)"/\1/')}"
ARCH="$(dpkg --print-architecture)"
STAGE="$ROOT/target/deb/thinsql_${VERSION}_${ARCH}"
OUT="$ROOT/target/thinsql_${VERSION}_${ARCH}.deb"

echo "==> 构建 release"
cargo build --release -p thinsql-app --manifest-path "$ROOT/Cargo.toml"

echo "==> 组装 $STAGE"
rm -rf "$STAGE"
install -Dm755 "$ROOT/target/release/thinsql-app" "$STAGE/usr/bin/thinsql-app"
install -Dm644 "$ROOT/packaging/thinsql.desktop" \
  "$STAGE/usr/share/applications/thinsql.desktop"

mkdir -p "$STAGE/DEBIAN"
cat > "$STAGE/DEBIAN/control" <<CONTROL
Package: thinsql
Version: ${VERSION}
Section: devel
Priority: optional
Architecture: ${ARCH}
Depends: libxkbcommon0, libvulkan1, libfontconfig1
Maintainer: ThinSQL
Description: Lightweight database client for MySQL and PostgreSQL
 A single-process native desktop client with no WebView or JVM runtime.
CONTROL

echo "==> 生成 $OUT"
dpkg-deb --build --root-owner-group "$STAGE" "$OUT"

echo "==> 完成"
dpkg-deb -I "$OUT" | sed -n '1,12p'
echo "文件大小: $(du -h "$OUT" | cut -f1)"
