#!/usr/bin/env bash
# 补测：从未运行过的分支 + 新改动 + 边界
#  A check 命令   B superfloppy（整盘 FS）   C resize --start 用法错误（非本命令选项）
#  D 缩容+--allow-move   E 边界（128 分区 / 紧贴 last_usable）
#  F mkfs 各 FS   G --sector-size 显式传参（镜像）
source "$(dirname "$0")/lib.sh"
require_bin



img T t27.img
img SF t27sf.img

rm -rf "$T" "$T"$SIDECAR_GLOB "$SF" "$SF"$SIDECAR_GLOB 2>/dev/null

mk() { # $1=fs  $2=size
  rm -rf "$T" "$T"$SIDECAR_GLOB
  truncate -s 1G "$T"
  $B new "$T" --yes >/dev/null
  $B add "$T" "$2" --align mib --name p1 --yes >/dev/null 2>&1
  $B mkfs "$T":1 "$1" --yes >/dev/null 2>&1
}

echo "########## A: check 命令（从未运行过的分支）##########"
mk ext4 300M
OUT=$($B check "$T":1 2>&1); echo "clean fs  -> exit=$? : $(echo "$OUT" | tail -1)"
# 破坏 ext4 超级块 → check 应报问题（非 0 或明确提示）
LD=$(lo_attach "$T"); mount_at "${LD}p1" "$TEST_MNT" && { umount "$TEST_MNT" 2>/dev/null; }
lo_detach "$LD"
printf 'XXXX' | dd of="$T" bs=1 seek=$((2048*512 + 1024)) conv=notrunc 2>/dev/null
OUT=$($B check "$T":1 2>&1); E=$?
echo "broken fs -> exit=$E : $(echo "$OUT" | tail -1)"
[ "$E" != "0" ] && echo "A CHECK-DETECTS-DAMAGE OK" || { echo "A damaged fs not reported (check output above)"; rc=1; }

echo
echo "########## B: superfloppy（无分区表，整盘 FS 扩容）##########"
rm -rf "$SF" "$SF"$SIDECAR_GLOB
truncate -s 200M "$SF"
mkfs.ext4 -q -F "$SF"
truncate -s 300M "$SF"      # 设备变大，FS 仍 200M
OUT=$($B resize "$SF" grow 2>&1); E=$?
echo "exit=$E : $(echo "$OUT" | tail -1)"
LD=$(lo_attach "$SF"); mount_at "$LD" "$TEST_MNT" 2>/dev/null && { echo "after: $(df -h --output=size "$TEST_MNT" | tail -1)（期望 ~293M）"; umount "$TEST_MNT" 2>/dev/null; }; lo_detach "$LD"
# 拒绝路径：unknown（全零盘）
rm -rf "$SF"; truncate -s 100M "$SF"
OUT=$($B resize "$SF" grow 2>&1); E=$?
echo "unknown -> exit=$E : $(echo "$OUT" | tail -1)"
[ "$E" = "3" ] && echo "B SUPERFLOPPY-REFUSAL OK" || { echo "B unexpected exit for unknown ($E)"; rc=1; }

echo
echo "########## C: resize --start 应报用法错误（非本命令选项）##########"
mk ext4 300M
OUT=$($B resize "$T":1 500M --start 4096 2>&1); E=$? # cli-surface: refused
echo "exit=$E : $(echo "$OUT" | tail -1)"
[ "$E" = "2" ] && echo "C USAGE-ERROR OK" || { echo "C WRONG EXIT ($E)"; rc=1; }

echo
echo "########## D: 缩容 + --allow-move（缩容不需要让位）##########"
mk ext4 500M
LD=$(lo_attach "$T"); mount_at "${LD}p1" "$TEST_MNT" && { dd if=/dev/urandom of="$TEST_MNT/d.bin" bs=1M count=50 2>/dev/null; umount "$TEST_MNT" 2>/dev/null; }
lo_detach "$LD"
OUT=$($B resize "$T":1 350M --allow-move --yes 2>&1); E=$?
echo "exit=$E : $(echo "$OUT" | tail -1)"
LD=$(lo_attach "$T")
e2fsck -fn "${LD}p1" >/dev/null 2>&1 && echo "D e2fsck clean" || { echo "D e2fsck ISSUES"; rc=1; }
mount_at "${LD}p1" "$TEST_MNT" 2>/dev/null && { echo "size: $(df -h --output=size "$TEST_MNT" | tail -1)"; umount "$TEST_MNT" 2>/dev/null; }
lo_detach "$LD"

echo
echo "########## E: 边界（128 分区上限 / 紧贴 last_usable）##########"
rm -rf "$T" "$T"$SIDECAR_GLOB
truncate -s 1G "$T"
$B new "$T" --yes >/dev/null
ok=0
for i in $(seq 1 128); do
  s=$((2048 + (i-1)*2048)); e=$((s + 2047))
  $B add "$T" $(( (e - s + 1) * 512 )) --start "$s" --name "p$i" >/dev/null 2>&1 && ok=$((ok+1)) || break
done
[ "$ok" = "128" ] && echo "created $ok partitions (128 = limit)" || { echo "created $ok partitions (want 128)"; rc=1; }
$B info "$T" | grep -o '"num":128[^}]*}' | head -1
# 第 129 个应被拒绝
$B add "$T" 1048576 --start 264192 --name over >/dev/null 2>&1; E=$?
[ "$E" != "0" ] && echo "129th add refused OK (exit=$E)" || { echo "129th add NOT refused (exit=$E)"; rc=1; }
# 紧贴：把 p128 扩到 last_usable，再加 1 扇区应被拒
# 1G 镜像末 LBA = 2097151；备份数组(32 扇区)位于末扇区的备份头之前，
# 故 last_usable = 2097151 - 32 - 1 = 2097118（同 boundary_last_usable.sh）
# --end 默认按 MiB 下取整，会把越界的 end 夹回可用区内（src/support.rs:258-278），
# 所以这里必须 --align none，越界请求才真正到达边界校验（src/movepart/resize.rs:404）
LU=2097118
echo "last_usable=$LU (2097151 - 32 - 1)"
# resize-part 只改分区表、不动 FS；--align none 让原始越界 LBA 直达边界校验
$B resize-part "$T":128 --align none --end "$LU" >/dev/null 2>&1; E=$?
[ "$E" = "0" ] && echo "grow-to-usable OK (exit=$E)" || { echo "grow-to-usable failed (exit=$E)"; rc=1; }
P=$($B info "$T" | grep -o '"num":128,[^}]*}' | grep -o '"last_lba":[0-9]*' | cut -d: -f2)
[ "$P" = "$LU" ] && echo "p128 at last_usable ($P)" || { echo "p128 not at last_usable (got $P, want $LU)"; rc=1; }
$B resize-part "$T":128 --align none --end "$((LU+1))" >/dev/null 2>&1; E=$?
[ "$E" != "0" ] && echo "beyond-usable refused OK (exit=$E)" || { echo "beyond-usable NOT refused (exit=$E)"; rc=1; }

echo
echo "########## F: mkfs 各 FS（add + mkfs）##########"
for fs in f2fs vfat exfat ntfs xfs btrfs; do
  sz=200M; [ "$fs" = "vfat" ] && sz=600M; [ "$fs" = "xfs" ] && sz=400M
  rm -rf "$T" "$T"$SIDECAR_GLOB
  truncate -s 1G "$T"
  $B new "$T" --yes >/dev/null
  $B add "$T" "$sz" --align mib --name t --yes >/dev/null 2>&1
  $B mkfs "$T":1 "$fs" --yes >/dev/null 2>&1; E=$?
  ID=$($B info "$T" | grep -o '"fs":"[^"]*"' | head -1)
  [ "$E" = "0" ] && [ -n "$ID" ] && echo "  $fs: mkfs OK ($ID)" || { echo "  $fs: mkfs FAILED (exit=$E, identified=$ID)"; rc=1; }
done

echo
echo "########## G: --sector-size 显式传参（镜像文件按 4096 布局）##########"
rm -rf "$T" "$T"$SIDECAR_GLOB
truncate -s 1G "$T"
$B new "$T" --yes --sector-size 4096 >/dev/null 2>&1; echo "new exit=$?"
$B info "$T" | grep -o '"sector_size":[0-9]*'
$B add "$T" 268435456 --start 256 --name p1 >/dev/null 2>&1; echo "add(256..65791) exit=$?"
$B info "$T" | grep -o '"num":1,[^}]*}'

echo
echo "########## 残留核对（cleanup 前）##########"
echo "root: $(ls "$WSL_ROOT" 2>/dev/null | wc -l)  loops: $(losetup -a | wc -l)"
exit $rc