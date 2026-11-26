#!/usr/bin/env bash
# Linux 构建环境准备（无需 root）
#
# 背景：gpui 在 Linux 上会链接 libxkbcommon / libxkbcommon-x11 / libxcb 等库，
# 链接阶段需要开发用的 `libfoo.so` 软链（由 -dev 包提供），而精简发行版或
# 容器里通常只有带 SONAME 的运行时文件（libfoo.so.0 / libfoo.so.1）。
#
# 链接器会从库文件自身读取 SONAME 并把它写进 DT_NEEDED，所以把 libfoo.so
# 指向任意一个已存在的 libfoo.so.N 就能完成链接，运行时仍按 SONAME 解析到
# 系统库，无需安装任何系统包。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
LIBDIR="$ROOT/native/lib"
SYS_LIBDIR="${SYS_LIBDIR:-/lib/x86_64-linux-gnu}"

mkdir -p "$LIBDIR"

link_one() {
  local name="$1" dst="$LIBDIR/$1.so" cand
  [ -e "$dst" ] && return 0
  # 运行时库的 SONAME 可能是 .so.0 到 .so.6 任意一个，按版本号排序取第一个
  for cand in $(ls -1 "$SYS_LIBDIR/$name".so "$SYS_LIBDIR/$name".so.* 2>/dev/null | sort -V); do
    if [ -e "$cand" ]; then
      ln -s "$cand" "$dst"
      echo "linked $dst -> $cand"
      return 0
    fi
  done
  echo "MISSING $name (no runtime library found in $SYS_LIBDIR)" >&2
}

for name in \
  libxkbcommon \
  libxkbcommon-x11 \
  libxcb \
  libX11 \
  libXau \
  libXdmcp \
  libfontconfig \
  libfreetype \
  ; do
  link_one "$name"
done

echo "done: $LIBDIR"
