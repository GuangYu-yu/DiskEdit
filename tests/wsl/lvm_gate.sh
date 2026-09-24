#!/usr/bin/env bash
# LVM 全链 Gate：多 LV 拒绝（--lv 缺省）/ --lv 指定 / 单 LV 自动选择 / lvextend -r 后 FS 真扩 /
# 镜像 PV 的 losetup 临时映射路径 / loop 无残留。
# 依赖 lvm2（pvcreate/vgcreate/lvcreate…）+ e2fsprogs —— WSL 未装 lvm2 时打印 SKIP 并退出 0
# （按约定不在 WSL 安装依赖以免弄脏环境；做法同 lvm_pv_online.sh）。
source "$(dirname "$0")/lib.sh"

if ! command -v pvcreate >/dev/null 2>&1; then
  echo "SKIP: lvm2 not installed (pvcreate missing) — 按约定不在 WSL 安装依赖"
  exit 0
fi

require_bin

fail() { echo "GATE FAIL: $*"; exit 1; }

# LVM 栈须先拆（LV→VG→PV），再释放本脚本的 loop（在 cleanup_all 的 rm 之前完成）
VG=""
cleanup_hook() {
  [ -n "$VG" ] && vgchange -an "$VG" 2>/dev/null
  [ -n "$VG" ] && vgremove -ff "$VG" 2>/dev/null
  [ -n "$LOOP" ] && lo_detach "$LOOP"
  [ -n "$LOOP2" ] && lo_detach "$LOOP2"
  return 0
}

lv_bytes() { blockdev --getsize64 "/dev/$1/$2"; }
fs_bytes() { dumpe2fs -h "/dev/$1/$2" 2>/dev/null | awk '/^Block count:/ {print $3 * 4096}'; }

# ---- 场景一：块设备 PV，VG 内两个 LV ----
IMG=/var/tmp/t31a.img
track_file "$IMG"
rm -f "$IMG" "$IMG".diskedit.* 2>/dev/null
# 128M：p1 80M +16M 的扩容终点（起始间隙 1MiB + 97MiB）必须装得下，
# 否则多 LV 拒绝用例会被"尺寸溢出"歪打正着地拒绝、--lv 链路在 grow 时超盘被拒
truncate -s 128M "$IMG"
$B new "$IMG" --yes >/dev/null || fail "new $IMG failed"
$B create "$IMG" --size 80M >/dev/null || fail "create $IMG failed"
LOOP=$(lo_attach "$IMG") || fail "losetup $IMG failed"
PART=${LOOP}p1
pvcreate -f "$PART" >/dev/null || fail "pvcreate $PART failed"
VG=gatevg$$
vgcreate "$VG" "$PART" >/dev/null || fail "vgcreate $VG failed"
lvcreate -L 16M -n lv1 "$VG" >/dev/null || fail "lvcreate lv1 failed"
lvcreate -L 8M -n lv2 "$VG" >/dev/null || fail "lvcreate lv2 failed"
mkfs.ext4 -F "/dev/$VG/lv1" >/dev/null || fail "mkfs.ext4 lv1 failed"
b1=$(lv_bytes "$VG" lv1) || fail "blockdev lv1 failed"
f1=$(fs_bytes "$VG" lv1) || fail "dumpe2fs lv1 failed"
{ [ "$b1" -eq 16777216 ] && [ "$f1" -eq "$b1" ]; } || fail "setup: lv1=$b1 fs=$f1 expected 16MiB both"

# 多 LV 且无 --lv → 拒绝（分区与 pvresize 已完成，属部分完成）
if out=$($B resize "${LOOP}:1" +16M --grow-lv 2>&1); then
    fail "multi-LV without --lv must not succeed: $out"
fi
b2=$(lv_bytes "$VG" lv1) || fail "blockdev lv1 (after refusal) failed"
[ "$b2" -eq "$b1" ] || fail "lv1 changed on refusal: $b2 != $b1"

# --lv 指定 lv1 → lv1 +16M，FS 同步真扩
$B resize "${LOOP}:1" +16M --grow-lv --lv lv1 >/dev/null || fail "--lv lv1 chain failed"
b3=$(lv_bytes "$VG" lv1) || fail "blockdev lv1 (after --lv) failed"
f3=$(fs_bytes "$VG" lv1) || fail "dumpe2fs lv1 (after --lv) failed"
[ "$b3" -eq "$((b1 + 16777216))" ] || fail "lv1 size: $b3 expected $((b1 + 16777216))"
[ "$f3" -eq "$b3" ] || fail "FS did not grow with LV: fs=$f3 lv=$b3 (lvextend -r broken?)"

# --lv vg/name 形式同样可用
$B resize "${LOOP}:1" +8M --grow-lv --lv "$VG/lv1" >/dev/null || fail "--lv vg/name failed"
[ "$(lv_bytes "$VG" lv1)" -eq "$((b3 + 8388608))" ] || fail "vg/name form did not extend"

# 移除 lv2 后单 LV 自动选择。
# 分区尺寸账：80M 起步，多 LV 拒绝用例按契约已扩 16M（部分完成），--lv lv1 再 +16M，
# vg/name 再 +8M → 此刻分区 120M；若再 +8M 会到 128M，加 1MiB 起始间隙恰好越盘，故收 +4M
lvremove -f "/dev/$VG/lv2" >/dev/null || fail "lvremove lv2 failed"
# 诊断转储：CI 曾在此处报 candidates "lv1, lv1"（lvs 输出重复行）——留存原始 JSON 以定性
lvs --reportformat json -o lv_name,lv_path,devices "$VG" 2>&1 | head -5
$B resize "${LOOP}:1" +4M --grow-lv >/dev/null || fail "single-LV auto-select failed"
[ "$(lv_bytes "$VG" lv1)" -eq "$((b3 + 8388608 + 4194304))" ] || fail "auto-select did not extend lv1"

lo_detach "$LOOP"; LOOP=
vgremove -ff "$VG" >/dev/null || fail "vgremove $VG failed"
VG=
rm -f "$IMG" "$IMG".diskedit.*; IMG=

# ---- 场景二：镜像内 PV（losetup 临时映射路径）----
IMG2=/var/tmp/t31b.img
track_file "$IMG2"
rm -f "$IMG2" "$IMG2".diskedit.* 2>/dev/null
truncate -s 64M "$IMG2"
$B new "$IMG2" --yes >/dev/null || fail "new $IMG2 failed"
$B create "$IMG2" --size 48M >/dev/null || fail "create $IMG2 failed"
LOOP2=$(lo_attach "$IMG2") || fail "losetup $IMG2 failed"
pvcreate -f "${LOOP2}p1" >/dev/null || fail "pvcreate ${LOOP2}p1 failed"
VG=gatevg2$$
vgcreate "$VG" "${LOOP2}p1" >/dev/null || fail "vgcreate $VG failed"
pv_before=$(pvs --noheadings --units b --nosuffix -o pv_size "${LOOP2}p1" 2>/dev/null | tr -d ' ' || true)
lo_detach "$LOOP2"; LOOP2=
[ -n "$pv_before" ] || fail "cannot read pv_size from ${IMG2}:1 before detach"
# 分区扩 8M → 镜像内 PV 自动 attach/detach 走 pvresize
$B resize "$IMG2:1" +8M >/dev/null || fail "image PV resize failed"
losetup -a | grep -F "$IMG2" && fail "loop device leaked after image PV resize"
# 重新 attach 验证 PV 尺寸确实变大（pvresize 已持久化到 PV 元数据）
LOOP2=$(lo_attach "$IMG2") || fail "re-losetup $IMG2 failed"
pv_after=$(pvs --noheadings --units b --nosuffix -o pv_size "${LOOP2}p1" | tr -d ' ') || fail "read pv_size after failed"
{ [ -n "$pv_before" ] && [ "$pv_after" -eq "$((pv_before + 8388608))" ]; } || fail "pv_size: $pv_after expected $((pv_before + 8388608))"
lo_detach "$LOOP2"; LOOP2=
vgremove -ff "$VG" >/dev/null || fail "vgremove $VG failed"
VG=
rm -f "$IMG2" "$IMG2".diskedit.*; IMG2=

echo "GATE PASS: all LVM chain cases verified"
