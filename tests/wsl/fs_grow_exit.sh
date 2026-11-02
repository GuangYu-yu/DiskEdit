#!/usr/bin/env bash
# 离线 FS 扩容（未挂载镜像）的真实工具路径与失败分类：
#   成功路径：真实 e2fsck + resize2fs 把 ext4 扩到新的分区末端，回读尺寸确实增长
#   非零退出：假 resize2fs 直接以非零退出（进程起过但没干活），分区已提交、文件系统未变，
#             按"后置条件未满足"报出（退出码 20，不算完成）
# 夹具是未挂载镜像：块设备分区在未挂载时被 diskedit 自己的 O_EXCL 句柄持有，
# e2fsck 会报 "is in use."（exit 8），走不到 resize2fs；镜像路径没有这个问题。
# 假工具靠工具表变体生效（工具解析在编译期钉死，调用环境的 PATH 不参与）。
# 非零退出用例只证明离线路径的文案与分类，不宣称文件系统被部分修改。
# 依赖 e2fsprogs —— WSL 未装时打印 SKIP 并退出 0（本脚本不自行安装依赖）。
source "$(dirname "$0")/lib.sh"

if ! command -v resize2fs >/dev/null 2>&1 || ! command -v mkfs.ext4 >/dev/null 2>&1; then
  echo "SKIP: e2fsprogs not installed — 按约定不在 WSL 安装依赖"
  exit 0
fi
require_fake_bin

IMG=/var/tmp/t62.img
LOOP=
PART0=
track_file "$IMG"

cleanup_hook() {
  [ -n "$LOOP" ] && lo_detach "$LOOP" 2>/dev/null
  rm -rf "$FAKE_TOOL_DIR"
}

fail() { echo "FAIL: $*"; exit 1; }

part_sz() { blockdev --getsize64 "${LOOP}p1"; }
fs_sz() { dumpe2fs -h "${LOOP}p1" 2>/dev/null | awk '/^Block count:/ {print $3 * 4096}'; }

# 64M 镜像、48M 分区、分区上未挂载的 ext4。测试只在准备与回读时挂 loop，
# resize 本身走 diskedit 自己的临时映射
build_fixture() {
  [ -n "$LOOP" ] && lo_detach "$LOOP"
  LOOP=
  rm -f "$IMG" "$IMG"$SIDECAR_GLOB 2>/dev/null
  truncate -s 64M "$IMG" || fail "truncate $IMG"
  $B new "$IMG" --yes >/dev/null || fail "new $IMG"
  $B create "$IMG" --size 48M >/dev/null || fail "create $IMG"
  LOOP=$(lo_attach "$IMG") || fail "losetup $IMG"
  lo_waitpart "${LOOP}p1" || fail "partition node ${LOOP}p1 not ready"
  mkfs.ext4 -F "${LOOP}p1" >/dev/null || fail "mkfs.ext4"
  PART0=$(part_sz) || fail "read partition size"
  FS0=$(fs_sz) || fail "read fs size"
  lo_detach "$LOOP"; LOOP=
}

echo "-- 成功路径：真实 e2fsck + resize2fs 把 FS 扩到新分区末端 --"
build_fixture
OUT=$($B resize "$IMG:1" 56M 2>&1); E=$?
echo "$OUT" | sed 's/^/    /'
exp "$E" 0 "分区与 FS 都扩成功"
LOOP=$(lo_attach "$IMG") || fail "re-losetup $IMG"
lo_waitpart "${LOOP}p1" || fail "partition node ${LOOP}p1 not ready"
PART1=$(part_sz) || fail "read partition size (after)"
FS1=$(fs_sz) || fail "read fs size (after)"
assert_eq "$PART1" "$((PART0 + 8388608))" "分区扩 8M"
if [ "$FS1" -gt "$FS0" ]; then echo "  OK   FS grew ($FS0 -> $FS1 bytes)"; else echo "  BAD  FS did not grow ($FS0 -> $FS1)"; rc=1; fi
if [ "$FS1" -le "$PART1" ] && [ "$FS1" -gt "$((PART1 - 65536))" ]; then
  echo "  OK   FS fills the new partition end"
else
  echo "  BAD  FS=$FS1 outside the new partition end $PART1"; rc=1
fi
e2fsck -fn "${LOOP}p1" >/dev/null 2>&1 && echo "  OK   ext4 e2fsck clean" || { echo "  BAD  ext4 e2fsck ISSUES"; rc=1; }
lo_detach "$LOOP"; LOOP=

echo "-- 非零退出：resize2fs 起过但未成功，分区已提交、FS 未变 --"
build_fixture
rm -rf "$FAKE_TOOL_DIR"; mkdir -p "$FAKE_TOOL_DIR"
printf '#!/bin/sh\necho "simulated resize2fs failure" >&2\nexit 1\n' > "$FAKE_TOOL_DIR/resize2fs"
chmod +x "$FAKE_TOOL_DIR/resize2fs"
OUT=$($BFK resize "$IMG:1" 56M 2>&1); E=$?
echo "$OUT" | sed 's/^/    /'
exp "$E" 20 "FS 步未完成 → 部分完成（20），不报完全成功"
case "$OUT" in
    *"resize2fs failed"*) echo "  OK   报出 resize2fs 的非零退出" ;;
    *) echo "  BAD  未报出 resize2fs 的失败"; rc=1 ;;
esac
case "$OUT" in
    *"simulated resize2fs failure"*) echo "  OK   失败原因（工具 stderr）随错误报出" ;;
    *) echo "  BAD  未带出工具 stderr"; rc=1 ;;
esac
case "$OUT" in
    *"e2fsck -fp"*) echo "  OK   待办给出前置检查命令（e2fsck -fp）" ;;
    *) echo "  BAD  未给出 e2fsck -fp 补救命令"; rc=1 ;;
esac
case "$OUT" in
    *"resize2fs <"*|*"resize2fs /"*) echo "  OK   待办给出 resize2fs 补救命令" ;;
    *) echo "  BAD  未给出 resize2fs 补救命令"; rc=1 ;;
esac
LOOP=$(lo_attach "$IMG") || fail "re-losetup $IMG"
lo_waitpart "${LOOP}p1" || fail "partition node ${LOOP}p1 not ready"
PART1=$(part_sz) || fail "read partition size (after)"
FS1=$(fs_sz) || fail "read fs size (after)"
assert_eq "$PART1" "$((PART0 + 8388608))" "分区已提交（8M）"
assert_eq "$FS1" "$FS0" "文件系统未被触碰"
e2fsck -fn "${LOOP}p1" >/dev/null 2>&1 && echo "  OK   ext4 e2fsck clean" || { echo "  BAD  ext4 e2fsck ISSUES"; rc=1; }
lo_detach "$LOOP"; LOOP=

rm -rf "$FAKE_TOOL_DIR"
exit $rc
