#!/usr/bin/env bash
# 后置条件契约的运行时验证：--no-fs / 全满足 / 缩容拒绝
#
# 去重归属：exfat 扩容拒绝、vfat 扩容 PARTIAL（fatresize 崩溃）这两个场景的唯一归属是
#          fs_f2fs_vfat_exfat_layout.sh（其中 exfat 拒绝还额外校验分区内数据哈希、更为严格）；
#          本脚本原先的两段重复用例已删，不再重复覆盖。
source "$(dirname "$0")/lib.sh"
require_bin

cleanup_hook() { rm -f /var/lib/diskedit/* 2>/dev/null; }

T=/var/tmp/t26.img
track_file "$T"

rm -f "$T" "$T"$SIDECAR_GLOB 2>/dev/null

mk() { # $1=fs  $2=size
  rm -f "$T" "$T"$SIDECAR_GLOB
  truncate -s 1G "$T"
  $B new "$T" --yes >/dev/null
  $B create "$T" --size "$2" --name p1 --fs "$1" >/dev/null 2>&1
}
pt() { $B info "$T" | grep -o '"num":1,[^}]*}' | grep -o '"size_bytes":[0-9]*'; }

echo "===== A: exfat + --no-fs → 只改布局 → OK ====="
mk exfat 400M
BEFORE=$(pt)
OUT=$($B resize "$T":1 900M --no-fs 2>&1); E=$?
echo "exit=$E  before=$BEFORE  after=$(pt)"
[ "$E" = "0" ] && echo "A OK" || { echo "A WRONG EXIT ($E)"; rc=1; }

echo
echo "===== B: ext4 扩容 → 后置条件全满足 → OK ====="
mk ext4 400M
LD=$(lo_attach "$T"); mount_at "${LD}p1" /testmnt && { dd if=/dev/urandom of=/testmnt/f.bin bs=1M count=100 2>/dev/null; MC=$(md5sum /testmnt/f.bin | cut -d' ' -f1); echo "before fs: $(df -h --output=size /testmnt | tail -1)"; umount /testmnt 2>/dev/null; }
lo_detach "$LD"
OUT=$($B resize "$T":1 900M 2>&1); E=$?
echo "exit=$E"
LD=$(lo_attach "$T"); mount_at "${LD}p1" /testmnt && { echo "after fs: $(df -h --output=size /testmnt | tail -1)"; [ "$MC" = "$(md5sum /testmnt/f.bin | cut -d' ' -f1)" ] && echo "B DATA OK" || { echo "B DATA CORRUPT"; rc=1; }; umount /testmnt 2>/dev/null; }
e2fsck -fn "${LD}p1" >/dev/null 2>&1 && echo "B e2fsck clean" || { echo "B e2fsck ISSUES"; rc=1; }
lo_detach "$LD"
[ "$E" = "0" ] && echo "B OK" || { echo "B WRONG EXIT ($E)"; rc=1; }

echo
echo "===== C: 缩容不支持的 FS → REFUSED 且不得改盘 ====="
mk exfat 800M
BEFORE=$(pt)
OUT=$($B resize "$T":1 400M 2>&1); E=$?
echo "exit=$E  before=$BEFORE  after=$(pt)"
echo "$OUT" | head -2
[ "$E" = "10" ] && echo "C REFUSED OK" || { echo "C WRONG EXIT ($E)"; rc=1; }
[ "$BEFORE" = "$(pt)" ] && echo "C DISK UNTOUCHED OK" || { echo "C DISK CHANGED (BAD)"; rc=1; }

echo
echo "===== 残留核对 ====="
echo "t26: $(ls /var/tmp/t26* 2>/dev/null | wc -l)  loops: $(losetup -a | wc -l)"
exit $rc