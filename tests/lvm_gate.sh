#!/usr/bin/env bash
# LVM 全链 Gate：须 Linux root + lvm2 + e2fsprogs，diskedit 二进制已构建。
# 用法：sudo bash tests/lvm_gate.sh [path/to/diskedit]
# 覆盖：多 LV 拒绝（--lv 缺省）/ --lv 指定 / 单 LV 自动选择 / lvextend -r 后
#       FS 真扩 / 镜像 PV losetup 路径 / loop 设备无残留。
set -euo pipefail

BIN=${1:-target/debug/diskedit}
[[ -x $BIN ]] || BIN=target/release/diskedit
[[ -x $BIN ]] || { echo "diskedit binary not found (build first)"; exit 1; }
[[ $(id -u) -eq 0 ]] || { echo "must run as root"; exit 1; }
for t in pvcreate vgcreate lvcreate lvremove pvresize losetup mkfs.ext4 dumpe2fs blockdev; do
    command -v "$t" >/dev/null || { echo "missing tool: $t"; exit 1; }
done

fail() { echo "GATE FAIL: $*"; exit 1; }
cleanup() {
    [[ -n ${VG:-} ]] && vgremove -ff "$VG" >/dev/null 2>&1
    [[ -n ${LOOP:-} ]] && losetup -d "$LOOP" >/dev/null 2>&1
    [[ -n ${IMG:-} ]] && rm -f "$IMG"
    [[ -n ${IMG2:-} ]] && rm -f "$IMG2"
}
trap cleanup EXIT

lv_bytes() { blockdev --getsize64 "/dev/$1/$2"; }
fs_bytes() { dumpe2fs -h "/dev/$1/$2" 2>/dev/null | awk '/^Block count:/ {print $3 * 4096}'; }

# ---- 场景一：块设备 PV，VG 内两个 LV ----
IMG=$(mktemp /tmp/diskedit-lvm-gate.XXXXXX.img)
truncate -s 96M "$IMG"
"$BIN" new "$IMG" --yes >/dev/null
"$BIN" create "$IMG" --size 80M >/dev/null
LOOP=$(losetup -P -f --show "$IMG")
PART=${LOOP}p1
pvcreate -f "$PART" >/dev/null
VG=gatevg$$
vgcreate "$VG" "$PART" >/dev/null
lvcreate -L 16M -n lv1 "$VG" >/dev/null
lvcreate -L 8M -n lv2 "$VG" >/dev/null
mkfs.ext4 -F "/dev/$VG/lv1" >/dev/null
b1=$(lv_bytes "$VG" lv1); f1=$(fs_bytes "$VG" lv1)
[[ $b1 -eq 16777216 && $f1 -eq $b1 ]] || fail "setup: lv1=$b1 fs=$f1 expected 16MiB both"

# 多 LV 且无 --lv → 拒绝（分区与 pvresize 已完成，属部分完成）
if out=$("$BIN" resize "${LOOP}:1" +16M --grow-lv 2>&1); then
    fail "multi-LV without --lv must not succeed: $out"
fi
b2=$(lv_bytes "$VG" lv1)
[[ $b2 -eq $b1 ]] || fail "lv1 changed on refusal: $b2 != $b1"

# --lv 指定 lv1 → lv1 +16M，FS 同步真扩
"$BIN" resize "${LOOP}:1" +16M --grow-lv --lv lv1 >/dev/null || fail "--lv lv1 chain failed"
b3=$(lv_bytes "$VG" lv1); f3=$(fs_bytes "$VG" lv1)
[[ $b3 -eq $((b1 + 16777216)) ]] || fail "lv1 size: $b3 expected $((b1 + 16777216))"
[[ $f3 -eq $b3 ]] || fail "FS did not grow with LV: fs=$f3 lv=$b3 (lvextend -r broken?)"

# --lv vg/name 形式同样可用
"$BIN" resize "${LOOP}:1" +8M --grow-lv --lv "$VG/lv1" >/dev/null || fail "--lv vg/name failed"
[[ $(lv_bytes "$VG" lv1) -eq $((b3 + 8388608)) ]] || fail "vg/name form did not extend"

# 移除 lv2 后单 LV 自动选择
lvremove -f "/dev/$VG/lv2" >/dev/null
"$BIN" resize "${LOOP}:1" +8M --grow-lv >/dev/null || fail "single-LV auto-select failed"
[[ $(lv_bytes "$VG" lv1) -eq $((b3 + 8388608 + 8388608)) ]] || fail "auto-select did not extend lv1"

losetup -d "$LOOP"; LOOP=
vgremove -ff "$VG" >/dev/null; VG=
rm -f "$IMG"; IMG=

# ---- 场景二：镜像内 PV（losetup 临时映射路径）----
IMG2=$(mktemp /tmp/diskedit-lvm-gate2.XXXXXX.img)
truncate -s 64M "$IMG2"
"$BIN" new "$IMG2" --yes >/dev/null
"$BIN" create "$IMG2" --size 48M >/dev/null
LOOP2=$(losetup -P -f --show "$IMG2")
pvcreate -f "${LOOP2}p1" >/dev/null
vgcreate "gatevg2$$" "${LOOP2}p1" >/dev/null
losetup -d "$LOOP2"
pv_before=$(pvs --noheadings --units b --nosuffix -o pv_size "${LOOP2}p1" 2>/dev/null | tr -d ' ' || echo detached)
# 分区扩 8M → 镜像内 PV 自动 attach/detach 走 pvresize
"$BIN" resize "$IMG2:1" +8M >/dev/null || fail "image PV resize failed"
losetup -a | grep -F "$IMG2" && fail "loop device leaked after image PV resize"
# 重新 attach 验证 PV 尺寸确实变大（pvresize 已持久化到 PV 元数据）
LOOP2=$(losetup -P -f --show "$IMG2")
pv_after=$(pvs --noheadings --units b --nosuffix -o pv_size "${LOOP2}p1" | tr -d ' ')
[[ -n $pv_before && $pv_after -eq $((pv_before + 8388608)) ]] || fail "pv_size: $pv_after expected $((pv_before + 8388608))"
losetup -d "$LOOP2"
rm -f "$IMG2"; IMG2=

echo "GATE PASS: all LVM chain cases verified"