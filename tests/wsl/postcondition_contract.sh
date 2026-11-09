#!/usr/bin/env bash
# 后置条件契约的运行时验证：--no-fs / 全满足 / 缩容拒绝
#
# 去重归属：exfat 扩容拒绝、vfat 扩容 PARTIAL（fatresize 崩溃）这两个场景的唯一归属是
#          fs_f2fs_vfat_exfat_layout.sh（其中 exfat 拒绝还额外校验分区内数据哈希、更为严格）；
#          这两个场景不在本脚本重复覆盖。
source "$(dirname "$0")/lib.sh"
require_bin


img T t26.img

rm -rf "$T" "$T"$SIDECAR_GLOB 2>/dev/null

mk() { # $1=fs  $2=size
  rm -rf "$T" "$T"$SIDECAR_GLOB
  truncate -s 1G "$T"
  $B new "$T" --yes >/dev/null
  $B add "$T" "$2" --align mib --name p1 --yes >/dev/null 2>&1
  $B mkfs "$T":1 "$1" --yes >/dev/null 2>&1
}
pt() { $B info "$T" | grep -o '"num":1,[^}]*}' | grep -o '"size_bytes":[0-9]*'; }

echo "===== A: exfat + --no-fs → 只改布局 → OK ====="
mk exfat 400M
BEFORE=$(pt)
OUT=$($B resize --yes "$T":1 900M --no-fs 2>&1); E=$?
echo "exit=$E  before=$BEFORE  after=$(pt)"
[ "$E" = "0" ] && echo "A OK" || { echo "A WRONG EXIT ($E)"; rc=1; }

echo
echo "===== B: ext4 扩容 → 后置条件全满足 → OK ====="
mk ext4 400M
LD=$(lo_attach "$T"); mount_at "${LD}p1" "$TEST_MNT" && { dd if=/dev/urandom of="$TEST_MNT/f.bin" bs=1M count=100 2>/dev/null; MC=$(mdsum "$TEST_MNT"/f.bin); echo "before fs: $(df -h --output=size "$TEST_MNT" | tail -1)"; umount "$TEST_MNT" 2>/dev/null; }
lo_detach "$LD"
OUT=$($B resize --yes "$T":1 900M 2>&1); E=$?
echo "exit=$E"
LD=$(lo_attach "$T"); mount_at "${LD}p1" "$TEST_MNT" && { echo "after fs: $(df -h --output=size "$TEST_MNT" | tail -1)"; [ "$MC" = "$(mdsum "$TEST_MNT"/f.bin)" ] && echo "B DATA OK" || { echo "B DATA CORRUPT"; rc=1; }; umount "$TEST_MNT" 2>/dev/null; }
e2fsck -fn "${LD}p1" >/dev/null 2>&1 && echo "B e2fsck clean" || { echo "B e2fsck ISSUES"; rc=1; }
lo_detach "$LD"
[ "$E" = "0" ] && echo "B OK" || { echo "B WRONG EXIT ($E)"; rc=1; }

echo
echo "===== C: 缩容不支持的 FS → UNSUPPORTED(6) 且不得改盘 ====="
mk exfat 800M
BEFORE=$(pt)
# 缩容入口是 resize-part（resize 只扩）：exfat 不可缩，FS 前置检查在写表之前拒绝
OUT=$($B resize-part --yes "$T":1 --end 821247 2>&1); E=$?
echo "exit=$E  before=$BEFORE  after=$(pt)"
echo "$OUT" | head -2
[ "$E" = "6" ] && echo "C UNSUPPORTED OK" || { echo "C WRONG EXIT ($E)"; rc=1; }
[ "$BEFORE" = "$(pt)" ] && echo "C DISK UNTOUCHED OK" || { echo "C DISK CHANGED (BAD)"; rc=1; }

echo
echo "===== D: mkfs 失败 → 分区已建、FS 未做 → PARTIAL(5) ====="
# 16M 远低于 mkfs.xfs 的 300M 下限：add 先把条目写进表，随后的 mkfs 越过擦除屏障即失败。
# 两条命令各自独立：add 的成功不再由 mkfs 的失败改写，故后置条件只按 mkfs 命令自身判定
rm -rf "$T" "$T"$SIDECAR_GLOB
truncate -s 1G "$T"
$B new "$T" --yes >/dev/null
$B add "$T" 16M --align mib --name tiny --yes >/dev/null 2>&1; exp "$?" 0 "add 16MiB tiny"
OUT=$($B mkfs "$T":1 xfs --yes 2>&1); E=$?
echo "exit=$E : $(echo "$OUT" | head -1)"
exp "$E" 5 "mkfs 失败 → 后置条件未全满足（PARTIAL）"
$B info "$T" | grep -q '"num":1' && echo "D PARTITION-CREATED OK（表已写）" || { echo "D partition missing (BAD)"; rc=1; }

echo
echo "===== 残留核对 ====="
echo "root: $(ls "$WSL_ROOT" 2>/dev/null | wc -l)  loops: $(losetup -a | wc -l)"
exit $rc