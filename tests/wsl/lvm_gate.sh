#!/usr/bin/env bash
# LVM 全链 Gate：多 LV 拒绝（--lv 缺省）/ --lv 指定 / 单 LV 自动选择 / lvextend -r 后 FS 真扩 /
# 镜像 PV 的 losetup 临时映射路径 / 活动 LV 持有 loop 分区时的 AllowDm 边界与 mkfs 拒绝 /
# 镜像 PV + 非活动 LV 的 --grow-lv 全链 / loop 无残留。
# 依赖 lvm2（pvcreate/vgcreate/lvcreate…）+ e2fsprogs —— WSL 未装 lvm2 时打印 SKIP 并退出 0
# （本脚本不自行安装依赖）。
source "$(dirname "$0")/lib.sh"

if ! command -v pvcreate >/dev/null 2>&1; then
  echo "SKIP: lvm2 not installed (pvcreate missing) — 按约定不在 WSL 安装依赖"
  exit 0
fi

require_bin

fail() { echo "GATE FAIL: $*"; exit 1; }

# LVM 栈须先拆（LV→VG→PV），再释放本脚本的 loop（在 cleanup_all 的 rm 之前完成）
VG=""
LOOP3=""
LOOP4=""
cleanup_hook() {
  [ -n "$VG" ] && vgchange -an "$VG" 2>/dev/null
  [ -n "$VG" ] && vgremove -ff "$VG" 2>/dev/null
  [ -n "$LOOP" ] && lo_detach "$LOOP"
  [ -n "$LOOP2" ] && lo_detach "$LOOP2"
  [ -n "$LOOP3" ] && lo_detach "$LOOP3"
  [ -n "$LOOP4" ] && lo_detach "$LOOP4"
  return 0
}

lv_bytes() { blockdev --getsize64 "/dev/$1/$2"; }
fs_bytes() { dumpe2fs -h "/dev/$1/$2" 2>/dev/null | awk '/^Block count:/ {print $3 * 4096}'; }

# ---- 场景一：块设备 PV，VG 内两个 LV ----
IMG=/var/tmp/t31a.img
track_file "$IMG"
rm -f "$IMG" "$IMG"$SIDECAR_GLOB 2>/dev/null
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

# 收尾顺序：LV 可能仍处活动态，须先 vgchange -an 释放 dm，vgremove 也必须在
# detach 之前——PV 所在 loop 一旦拆掉，lvm 扫不到 VG，vgremove 报 "not found"
vgchange -an "$VG" >/dev/null 2>&1
vgremove -ff "$VG" >/dev/null || fail "vgremove $VG failed"
lo_detach "$LOOP" || fail "lo_detach $LOOP failed"; LOOP=
VG=
rm -f "$IMG" "$IMG"$SIDECAR_GLOB; IMG=

# ---- 场景二：镜像内 PV（losetup 临时映射路径）----
IMG2=/var/tmp/t31b.img
track_file "$IMG2"
rm -f "$IMG2" "$IMG2"$SIDECAR_GLOB 2>/dev/null
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
# vgremove 必须在 detach 之前（PV 在 loop 上，先拆 VG 就找不到了）
vgremove -ff "$VG" >/dev/null || fail "vgremove $VG failed"
lo_detach "$LOOP2"; LOOP2=
VG=
rm -f "$IMG2" "$IMG2"$SIDECAR_GLOB; IMG2=

# ---- 场景三：镜像内 PV + 活动 LV（loop 分区被 dm 持有）----
# 这一场景检验 ImageLoopPolicy::AllowDm / with_partition_device_for_lvm 的两个边界：
#   a) 活动的 LV 使 dm 持有 loop 分区时，通用占用规则不得把 LVM 链误判成"被别的栈占用"（exit 10）
#   b) 整段重写（mkfs）在同一状态下仍须被 REFUSED(10) 拒绝，且不动盘上字节
# 另记录 LVM 的一个运行期事实：同一 PV 经两个 loop 可见时，LVM 偏好"已被 LV 使用"的那个，
# 对临时映射的 pvresize 是 no-op 且退出码为 0。工具的后置条件检查因此把它报成 PARTIAL(20)
# （LVM 链待办），不宣称成功；表项只向右扩展，PV/LV/FS 均不变。
IMG3=/var/tmp/t31c.img
track_file "$IMG3"
rm -f "$IMG3" "$IMG3"$SIDECAR_GLOB 2>/dev/null
truncate -s 96M "$IMG3"
$B new "$IMG3" --yes >/dev/null || fail "new $IMG3 failed"
$B create "$IMG3" --size 48M >/dev/null || fail "create $IMG3 failed"
LOOP3=$(lo_attach "$IMG3") || fail "losetup $IMG3 failed"
pvcreate -f "${LOOP3}p1" >/dev/null || fail "pvcreate ${LOOP3}p1 failed"
VG=gatevg3$$
vgcreate "$VG" "${LOOP3}p1" >/dev/null || fail "vgcreate $VG failed"
lvcreate -L 16M -n lv1 "$VG" >/dev/null || fail "lvcreate lv1 failed"
mkfs.ext4 -F "/dev/$VG/lv1" >/dev/null || fail "mkfs.ext4 lv1 failed"
lv_before=$(lv_bytes "$VG" lv1) || fail "blockdev lv1 failed"
fs_before=$(fs_bytes "$VG" lv1) || fail "dumpe2fs lv1 failed"
last_before=$($B info "$IMG3" | grep -o '"last_lba":[0-9]*' | head -1 | cut -d: -f2)
pv_before=$(pvs --noheadings --units b --nosuffix -o pv_size "${LOOP3}p1" 2>/dev/null | tr -d ' ')
[ -n "$pv_before" ] || fail "cannot read pv_size from ${IMG3}:1"

# b) 先验证整段重写被拒绝：活动的 LV 仍在，mkfs 不得动 PV 的字节
$B mkfs "$IMG3:1" ext4 --yes >/dev/null 2>&1
exp "$?" 10 "活动 LV 持有分区时 mkfs 被拒"
pvs --noheadings -o pv_name "${LOOP3}p1" >/dev/null 2>&1 || fail "PV destroyed by refused mkfs"

# a) 通用占用规则不得拒绝 LVM 链本身
out=$($B resize "$IMG3:1" +8M --grow-lv --lv lv1 2>&1); got=$?
case "$out" in
    *"held by"*) fail "generic holder gate rejected the LVM chain: $out" ;;
esac
[ "$got" -ne 10 ] || fail "AllowDm gate refused the LVM chain (exit 10): $out"
[ "$got" -eq 20 ] || fail "expected PARTIAL(20) from duplicate-PV handling, got $got: $out"
case "$out" in
    *"pending"*) ;;
    *) fail "partial result not reported as pending: $out" ;;
esac
last_after=$($B info "$IMG3" | grep -o '"last_lba":[0-9]*' | head -1 | cut -d: -f2)
[ "$last_after" -eq "$((last_before + 16384))" ] || fail "table not extended: $last_before -> $last_after"
[ "$(lv_bytes "$VG" lv1)" -eq "$lv_before" ] || fail "LV changed despite no-op pvresize"
[ "$(fs_bytes "$VG" lv1)" -eq "$fs_before" ] || fail "LV filesystem changed despite no-op pvresize"
pv_after=$(pvs --noheadings --units b --nosuffix -o pv_size "${LOOP3}p1" 2>/dev/null | tr -d ' ')
[ "$pv_after" -eq "$pv_before" ] || fail "PV resized behind the active stack: $pv_before -> $pv_after"
vgdisplay "$VG" >/dev/null 2>&1 || fail "VG damaged"
[ "$(losetup -a | grep -cF "$IMG3")" -eq 1 ] || fail "temporary loop leaked for $IMG3"

vgchange -an "$VG" >/dev/null 2>&1
vgremove -ff "$VG" >/dev/null || fail "vgremove $VG failed"
lo_detach "$LOOP3"; LOOP3=
VG=
rm -f "$IMG3" "$IMG3"$SIDECAR_GLOB; IMG3=

# ---- 场景四：镜像内 PV + 非活动 LV，--grow-lv 全链 ----
# 无 dm 持有者时走 with_partition_device_for_lvm 的正常路径：pvresize 吸收新增空间，
# lvextend -r 把同一份增量传给 LV 与 FS。与场景三合起来覆盖该入口的放行与成功两条路径。
IMG4=/var/tmp/t31d.img
track_file "$IMG4"
rm -f "$IMG4" "$IMG4"$SIDECAR_GLOB 2>/dev/null
truncate -s 64M "$IMG4"
$B new "$IMG4" --yes >/dev/null || fail "new $IMG4 failed"
$B create "$IMG4" --size 48M >/dev/null || fail "create $IMG4 failed"
LOOP4=$(lo_attach "$IMG4") || fail "losetup $IMG4 failed"
lo_waitpart "${LOOP4}p1" || fail "partition node ${LOOP4}p1 not ready"
pvcreate -f "${LOOP4}p1" >/dev/null || fail "pvcreate ${LOOP4}p1 failed"
VG=gatevg4$$
vgcreate "$VG" "${LOOP4}p1" >/dev/null || fail "vgcreate $VG failed"
lvcreate -L 16M -n lv1 "$VG" >/dev/null || fail "lvcreate lv1 failed"
mkfs.ext4 -F "/dev/$VG/lv1" >/dev/null || fail "mkfs.ext4 lv1 failed"
lv_before=$(lv_bytes "$VG" lv1) || fail "blockdev lv1 failed"
pv_before=$(pvs --noheadings --units b --nosuffix -o pv_size "${LOOP4}p1" 2>/dev/null | tr -d ' ')
[ -n "$pv_before" ] || fail "cannot read pv_size from ${IMG4}:1"
# 先停用 VG 再拆 loop：起始状态无 dm 持有者
vgchange -an "$VG" >/dev/null 2>&1 || fail "vgchange -an failed"
lo_detach "$LOOP4"; LOOP4=

$B resize "$IMG4:1" +8M --grow-lv --lv lv1 >/dev/null || fail "image PV --grow-lv chain failed"
# lvextend -r 需要设备节点；LV 若被 LVM 留在非活动态，这里显式激活再取值
lvchange -ay "$VG/lv1" >/dev/null 2>&1
lv_after=$(lv_bytes "$VG" lv1) || fail "blockdev lv1 (after) failed"
fs_after=$(fs_bytes "$VG" lv1) || fail "dumpe2fs lv1 (after) failed"
assert_eq "$lv_after" "$((lv_before + 8388608))" "LV 扩 8M"
[ "$fs_after" -eq "$lv_after" ] || fail "FS did not grow with LV: fs=$fs_after lv=$lv_after"

# 停用后 dm 释放持有，临时 loop 的 autoclear 生效；再挂一次读 PV 元数据
vgchange -an "$VG" >/dev/null 2>&1
for _ in {1..20}; do [ -z "$(losetup -j "$IMG4" -n -O NAME)" ] && break; sleep 0.1; done
LOOP4=$(lo_attach "$IMG4") || fail "re-losetup $IMG4 failed"
lo_waitpart "${LOOP4}p1" || fail "partition node ${LOOP4}p1 not ready"
pv_after=$(pvs --noheadings --units b --nosuffix -o pv_size "${LOOP4}p1" 2>/dev/null | tr -d ' ')
assert_eq "$pv_after" "$((pv_before + 8388608))" "PV 吸收新增 8M"
vgremove -ff "$VG" >/dev/null || fail "vgremove $VG failed"
lo_detach "$LOOP4"; LOOP4=
VG=
rm -f "$IMG4" "$IMG4"$SIDECAR_GLOB; IMG4=

echo "GATE PASS: all LVM chain cases verified"