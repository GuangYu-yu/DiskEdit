#!/usr/bin/env bash
# 分区已提交、LVM 收尾未完成时的续跑：checkpoint 必须保留，重跑按 Committed 续做，
# LV 只达到同一目标一次。
#
# 夹具是镜像路径（$IMG:1）：分区的 PV 由一只 offset+sizelimit 映射承载，LV 保持活动
# （--grow-lv 只接受活动 LV），dm 因此持有该映射。这只映射就是上一轮留下的现场——
# diskedit 的预检与收尾共用它，不另建第二只 loop（对同一段字节建两只映射会让 LVM 看到
# duplicate PV，pvresize 静默无效）。
#
# 失败由假 lvextend 确定性注入（工具表在编译期钉死，调用环境的 PATH 不参与解析，
# 故用 $BFK：假目录优先、其余照常）：
#   现场一：lvextend 未执行（假工具直接失败）——LV 尚未扩容
#   现场二：lvextend 已执行（假工具先调真工具）后报错——LV 已达到目标，核验前退出
# 两种现场的第二次运行都由假工具放行，落到真 lvextend / 真 lvs。
# 依赖 lvm2 与 e2fsprogs——WSL 未装 lvm2 时打印 SKIP 并退出 0（本脚本不自行安装依赖）。
source "$(dirname "$0")/lib.sh"

if ! command -v pvcreate >/dev/null 2>&1; then
  echo "SKIP: lvm2 not installed (pvcreate missing) — 按约定不在 WSL 安装依赖"
  exit 0
fi
require_fake_bin

REAL_LVEXTEND=$(command -v lvextend) || { echo "SKIP: lvextend missing"; exit 0; }
IMG=/var/tmp/t61.img
MARK=/var/tmp/t61.mark
VG=lvresume$$
LOOP=
track_file "$IMG"

cleanup_hook() {
  vgchange -an "$VG" >/dev/null 2>&1
  [ -n "$LOOP" ] && lo_detach "$LOOP" 2>/dev/null
  vgremove -ff "$VG" >/dev/null 2>&1
  rm -f "$MARK"
  rm -rf "$FAKE_TOOL_DIR"
}

rc=0
fail() { echo "FAIL: $*"; exit 1; }
bad() { echo "  BAD  $*"; rc=1; }

lv_sz() { lvs --noheadings --units b --nosuffix -o lv_size "/dev/$VG/lv1" 2>/dev/null | tr -d ' '; }
fs_sz() { dumpe2fs -h "/dev/$VG/lv1" 2>/dev/null | awk '/^Block count:/ {print $3 * 4096}'; }
part_sz() { blockdev --getsize64 "$LOOP"; }

# 假工具：$1=名字，$2=脚本正文（正文里可用 $MARK / $REAL_LVEXTEND）
mkfake() {
  rm -rf "$FAKE_TOOL_DIR"; mkdir -p "$FAKE_TOOL_DIR"
  printf '#!/bin/sh\n%s\n' "$2" > "$FAKE_TOOL_DIR/$1"
  chmod +x "$FAKE_TOOL_DIR/$1"
}

# 现场：64M 镜像、48M 分区上的 PV/VG/LV(16M,ext4)。分区起点由内核分区节点读出，
# PV 建在该起点的 offset 映射上，LV 保持活动——dm 持有这只映射，正是收尾要复用的现场
build_fixture() {
  if [ -n "$LOOP" ]; then
    vgchange -an "$VG" >/dev/null 2>&1
    lo_detach "$LOOP"
    LOOP=
  fi
  vgremove -ff "$VG" >/dev/null 2>&1
  rm -f "$IMG" "$IMG"$SIDECAR_GLOB "$MARK" 2>/dev/null
  truncate -s 64M "$IMG" || fail "truncate $IMG"
  $B new "$IMG" --yes >/dev/null || fail "new $IMG"
  $B create "$IMG" --size 48M >/dev/null || fail "create $IMG"
  local probe pn start size
  probe=$(lo_attach "$IMG") || fail "losetup $IMG"
  lo_waitpart "${probe}p1" || fail "partition node ${probe}p1 not ready"
  pn=$(basename "${probe}p1")
  start=$(( $(cat "/sys/class/block/$pn/start") * 512 )) || fail "read partition start"
  size=$(( $(cat "/sys/class/block/$pn/size") * 512 )) || fail "read partition size"
  lo_detach "$probe"
  for _ in {1..20}; do [ -z "$(losetup -j "$IMG" -n -O NAME 2>/dev/null)" ] && break; sleep 0.1; done
  LOOP=$(losetup -o "$start" --sizelimit "$size" -f --show "$IMG") || fail "losetup offset $IMG"
  pvcreate -f "$LOOP" >/dev/null || fail "pvcreate"
  vgcreate "$VG" "$LOOP" >/dev/null || fail "vgcreate"
  lvcreate -L 16M -n lv1 "$VG" >/dev/null || fail "lvcreate"
  mkfs.ext4 -F "/dev/$VG/lv1" >/dev/null || fail "mkfs.ext4"
  PART0=$(part_sz) || fail "read partition size"
  LV0=$(lv_sz) || fail "read lv size"
  [ -n "$LV0" ] || fail "empty lv size"
}

# 收尾状态：分区/LV/FS 尺寸与 checkpoint 是否在场
report_state() { # $1=期望的 LV 字节, $2=标签
  local lv fs part
  lv=$(lv_sz); fs=$(fs_sz); part=$(part_sz)
  echo "  state: part=$part lv=$lv fs=$fs"
  assert_eq "$part" "$((PART0 + 8388608))" "$2: partition grew by 8M"
  assert_eq "$lv" "$1" "$2: LV size"
  if [ "$fs" = "$1" ]; then echo "  OK   $2: FS grew with the LV ($fs)"; else bad "$2: fs=$fs want=$1"; fi
  e2fsck -fn "/dev/$VG/lv1" >/dev/null 2>&1 && echo "  OK   $2: LV e2fsck clean" || bad "$2: LV e2fsck ISSUES"
  if [ -e "$IMG$CKPT_SUFFIX" ]; then echo "  state: checkpoint present ($(stat -c %s "$IMG$CKPT_SUFFIX") bytes)"; else echo "  state: checkpoint absent"; fi
  # 收尾链确实在同一只映射上跑过：duplicate PV 下 pvresize 静默无效，LV 到不了目标。
  # 同时打印映射的起点与 sizelimit，证明收尾是把同一只映射的 sizelimit 抬到新分区长度，
  # 而不是另建第二只 loop
  echo "  state: loops on image = $(losetup -j "$IMG" -n -O NAME,OFFSET,SIZELIMIT | tr '\n' ' ')"
  echo "  state: loop capacity = $(blockdev --getsize64 "$LOOP") bytes"
}

expect_ckpt() {
  if [ -e "$IMG$CKPT_SUFFIX" ]; then echo "  OK   $1: checkpoint kept"; else bad "$1: checkpoint removed"; fi
}
expect_no_ckpt() {
  if [ -e "$IMG$CKPT_SUFFIX" ]; then bad "$1: checkpoint left behind"; else echo "  OK   $1: checkpoint removed"; fi
}

echo "-- 现场一：lvextend 未执行（LV 尚未扩容）--"
build_fixture
mkfake lvextend "if [ ! -e \"$MARK\" ]; then
  : > \"$MARK\"
  echo 'simulated lvextend failure: nothing executed' >&2
  exit 1
fi
exec \"$REAL_LVEXTEND\" \"\$@\""
OUT=$($BFK resize "$IMG:1" 56M --grow-lv --lv lv1 2>&1); E=$?
echo "$OUT" | sed 's/^/    /'
exp "$E" 20 "表已提交、链失败 → 部分完成（20）"
case "$OUT" in *"simulated lvextend failure"*) echo "  OK   链失败原因随待办报出";; *) bad "未报出 lvextend 失败原因";; esac
expect_ckpt "现场一"
report_state "$LV0" "现场一/首次"   # LV 未被扩容
OUT=$($BFK resize "$IMG:1" 56M --grow-lv --lv lv1 2>&1); E=$?
echo "$OUT" | sed 's/^/    /'
exp "$E" 0 "重跑补做未完成的链"
report_state "$((LV0 + 8388608))" "现场一/重跑"
expect_no_ckpt "现场一/重跑"

echo "-- 现场二：lvextend 已执行，核验前退出（LV 已达到目标）--"
build_fixture
mkfake lvextend "if [ ! -e \"$MARK\" ]; then
  \"$REAL_LVEXTEND\" \"\$@\" || exit \$?
  : > \"$MARK\"
  echo 'simulated verification failure after lvextend' >&2
  exit 1
fi
exec \"$REAL_LVEXTEND\" \"\$@\""
OUT=$($BFK resize "$IMG:1" 56M --grow-lv --lv lv1 2>&1); E=$?
echo "$OUT" | sed 's/^/    /'
exp "$E" 20 "LV 已扩但核验失败 → 部分完成（20）"
case "$OUT" in *"simulated verification failure"*) echo "  OK   核验失败原因随待办报出";; *) bad "未报出核验失败原因";; esac
expect_ckpt "现场二"
report_state "$((LV0 + 8388608))" "现场二/首次"   # LV 已到目标
OUT=$($BFK resize "$IMG:1" 56M --grow-lv --lv lv1 2>&1); E=$?
echo "$OUT" | sed 's/^/    /'
exp "$E" 0 "重跑确认已在目标、不重复扩"
report_state "$((LV0 + 8388608))" "现场二/重跑"
expect_no_ckpt "现场二/重跑"

vgchange -an "$VG" >/dev/null 2>&1
if [ -n "$LOOP" ]; then lo_detach "$LOOP"; LOOP=; fi
vgremove -ff "$VG" >/dev/null 2>&1
rm -rf "$FAKE_TOOL_DIR"
rm -f "$MARK"
exit $rc
