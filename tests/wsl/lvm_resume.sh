#!/usr/bin/env bash
# 分区已提交、LVM 收尾未完成时的续跑：checkpoint 必须保留，重跑按 Committed 续做，
# LV 只达到同一目标一次。
#
# 夹具是镜像路径（$IMG:1）：PV 建在分区起点上的一只 offset 映射里，映射的 sizelimit 恰好
# 等于当时的分区长度。复用判据要求起点与长度上限都精确等于本次请求（mapping_covers），
# 差一个字节都会被当成另一只映射。LV 保持活动（--grow-lv 只接受活动 LV），dm 因此持有
# 该映射。
#
# 首次运行因此停在"映射还没覆盖新边界"：表写成了 56M，收尾链要的正是 56M，而 dm 持有的
# 映射仍停在 48M；工具不改不属于自己的 loop 的 sizelimit（没有恢复点），把这一步作为待办
# 报出（退出 5，checkpoint 保留）。随后由持有它的栈（这里的 VG）自行解绑，并按表里的新
# 长度重绑；此后的预检与收尾都落在这同一只已覆盖新边界的映射上，不另建第二只 loop
#（对同一段字节建两只映射会让 LVM 看到 duplicate PV，pvresize 静默无效）。
#
# 失败由假 lvextend 确定性注入（工具表在编译期钉死，调用环境的 PATH 不参与解析，
# 故用 $BFK：假目录优先、其余照常）：
#   现场一：重绑后的重跑 lvextend 未执行（假工具直接失败）——LV 尚未扩容
#   现场二：重绑后的重跑 lvextend 已执行（假工具先调真工具）后报错——LV 已达到目标
# 放行时落到真 lvextend / 真 lvs。
# 依赖 lvm2 与 e2fsprogs——WSL 未装 lvm2 时打印 SKIP 并退出 0（本脚本不自行安装依赖）。
source "$(dirname "$0")/lib.sh"

if ! command -v pvcreate >/dev/null 2>&1; then
  echo "SKIP: lvm2 not installed (pvcreate missing) — 按约定不在 WSL 安装依赖"
  exit 0
fi
require_fake_bin

REAL_LVEXTEND=$(command -v lvextend) || { echo "SKIP: lvextend missing"; exit 0; }
img IMG t61.img
MARK=$WSL_ROOT/t61.mark
VG=lvresume$$
LOOP=
START=

cleanup_hook() {
  vgchange -an "$VG" >/dev/null 2>&1
  [ -n "$LOOP" ] && lo_detach "$LOOP" 2>/dev/null
  vgremove -ff "$VG" >/dev/null 2>&1
  rm -rf "$MARK"
  rm -rf "$FAKE_TOOL_DIR"
}

rc=0
fail() { echo "FAIL: $*"; exit 1; }
bad() { echo "  BAD  $*"; rc=1; }

lv_sz() { lvs --noheadings --units b --nosuffix -o lv_size "/dev/$VG/lv1" 2>/dev/null | tr -d ' '; }
fs_sz() { dumpe2fs -h "/dev/$VG/lv1" 2>/dev/null | awk '/^Block count:/ {print $3 * 4096}'; }
# 分区长度取自镜像里的表：映射的 sizelimit 要与表里的分区长度精确相等，不能用映射容量代替
part_sz() {
  $B info "$IMG" 2>/dev/null |
    sed -n 's/.*"num":1,"first_lba":[0-9]*,"last_lba":[0-9]*,"size_bytes":\([0-9]*\).*/\1/p'
}

# 假工具：$1=名字，$2=脚本正文（正文里可用 $MARK / $REAL_LVEXTEND）
mkfake() {
  rm -rf "$FAKE_TOOL_DIR"; mkdir -p "$FAKE_TOOL_DIR"
  printf '#!/bin/sh\n%s\n' "$2" > "$FAKE_TOOL_DIR/$1"
  chmod +x "$FAKE_TOOL_DIR/$1"
}

# 等镜像上的 loop 全部消失（losetup -d 是异步的，紧接着的 -f 可能拿不到同一个号）
wait_no_loop() {
  for _ in {1..20}; do
    [ -z "$(losetup -j "$IMG" -n -O NAME 2>/dev/null)" ] && return 0
    sleep 0.1
  done
  return 1
}

# 现场：64M 镜像、48M 分区上的 PV/VG/LV(16M,ext4)。分区起点由内核分区节点读出，
# PV 建在该起点、长度精确等于表里分区长度的那只映射上，LV 保持活动——dm 持有这只映射
build_fixture() {
  if [ -n "$LOOP" ]; then
    vgchange -an "$VG" >/dev/null 2>&1
    lo_detach "$LOOP"
    LOOP=
  fi
  vgremove -ff "$VG" >/dev/null 2>&1
  rm -rf "$IMG" "$IMG"$SIDECAR_GLOB "$MARK" 2>/dev/null
  truncate -s 64M "$IMG" || fail "truncate $IMG"
  $B new "$IMG" --yes >/dev/null || fail "new $IMG"
  $B add "$IMG" 48M --align mib --yes >/dev/null || fail "add $IMG"
  local probe pn start len
  probe=$(lo_attach "$IMG") || fail "losetup $IMG"
  lo_waitpart "${probe}p1" || fail "partition node ${probe}p1 not ready"
  pn=$(basename "${probe}p1")
  start=$(( $(cat "/sys/class/block/$pn/start") * 512 )) || fail "read partition start"
  lo_detach "$probe"
  wait_no_loop || fail "probe loop did not detach"
  len=$(part_sz) || fail "read partition size"
  [ -n "$len" ] || fail "empty partition size"
  START=$start
  LOOP=$(losetup -o "$start" --sizelimit "$len" -f --show "$IMG") || fail "losetup offset $IMG"
  pvcreate -f "$LOOP" >/dev/null || fail "pvcreate"
  vgcreate "$VG" "$LOOP" >/dev/null || fail "vgcreate"
  lvcreate -L 16M -n lv1 "$VG" >/dev/null || fail "lvcreate"
  mkfs.ext4 -F "/dev/$VG/lv1" >/dev/null || fail "mkfs.ext4"
  PART0=$(part_sz) || fail "read partition size"
  LV0=$(lv_sz) || fail "read lv size"
  [ -n "$LV0" ] || fail "empty lv size"
}

# 表增长之后由持有映射的栈自己解绑、按表里的新长度重绑：sizelimit 归栈管，工具不改它。
# 重绑后预检（按表里的新长度）与收尾链落在同一只已覆盖新边界的映射上
rebind_loop() {
  local len
  vgchange -an "$VG" >/dev/null 2>&1
  lo_detach "$LOOP"
  wait_no_loop || fail "old loop did not detach"
  len=$(part_sz) || fail "read partition size"
  [ -n "$len" ] || fail "empty partition size"
  LOOP=$(losetup -o "$START" --sizelimit "$len" -f --show "$IMG") || fail "rebind $IMG"
  vgchange -ay "$VG" >/dev/null 2>&1 || fail "vgchange -ay $VG"
  for _ in {1..20}; do [ -b "/dev/$VG/lv1" ] && return 0; sleep 0.1; done
  fail "LV node not ready after rebind"
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
  if record_present "$IMG"; then echo "  state: record present (cursor=$(record_cursor "$IMG"))"; else echo "  state: record absent"; fi
  # 打印全部映射与当前容量：收尾链必须落在唯一那只、且已覆盖新边界的映射上
  echo "  state: loops on image = $(losetup -j "$IMG" -n -O NAME,OFFSET,SIZELIMIT | tr '\n' ' ')"
  echo "  state: loop capacity = $(blockdev --getsize64 "$LOOP") bytes"
}

expect_record() {
  if record_present "$IMG"; then echo "  OK   $1: record kept"; else bad "$1: record missing"; fi
}
expect_no_record() {
  if record_absent "$IMG"; then echo "  OK   $1: record removed"; else bad "$1: record left behind"; fi
}
expect_sizelimit_pending() {
  case "$2" in
    *"does not change the size limit"*) echo "  OK   $1: 待办原因是映射未覆盖新边界";;
    *) bad "$1: 未报出映射边界原因";;
  esac
}

# 首次运行：表已提交，链要的新边界超出 dm 持有的映射 → 待办，5
first_run_pending() { # $1=标签
  local out e
  out=$($B resize "$IMG:1" 56M --grow-lv --lv lv1 2>&1); e=$?
  echo "$out" | sed 's/^/    /'
  exp "$e" 5 "$1: 表已提交、映射未覆盖新边界 → 部分完成（5）"
  expect_sizelimit_pending "$1" "$out"
  expect_record "$1"
  report_state "$LV0" "$1"
}

echo "-- 现场一：重绑后 lvextend 未执行（LV 尚未扩容）--"
build_fixture
mkfake lvextend "if [ ! -e \"$MARK\" ]; then
  : > \"$MARK\"
  echo 'simulated lvextend failure: nothing executed' >&2
  exit 1
fi
exec \"$REAL_LVEXTEND\" \"\$@\""
first_run_pending "现场一/首次"
rebind_loop
OUT=$($BFK resize "$IMG:1" 56M --grow-lv --lv lv1 2>&1); E=$?
echo "$OUT" | sed 's/^/    /'
exp "$E" 5 "链失败（lvextend 未执行）→ 部分完成（5）"
case "$OUT" in *"simulated lvextend failure"*) echo "  OK   链失败原因随待办报出";; *) bad "未报出 lvextend 失败原因";; esac
expect_record "现场一/注入"
report_state "$LV0" "现场一/注入"   # LV 未被扩容
OUT=$($BFK resize "$IMG:1" 56M --grow-lv --lv lv1 2>&1); E=$?
echo "$OUT" | sed 's/^/    /'
exp "$E" 0 "重跑补做未完成的链"
report_state "$((LV0 + 8388608))" "现场一/重跑"
expect_no_record "现场一/重跑"

echo "-- 现场二：重绑后 lvextend 已执行，核验前退出（LV 已达到目标）--"
build_fixture
mkfake lvextend "if [ ! -e \"$MARK\" ]; then
  \"$REAL_LVEXTEND\" \"\$@\" || exit \$?
  : > \"$MARK\"
  echo 'simulated verification failure after lvextend' >&2
  exit 1
fi
exec \"$REAL_LVEXTEND\" \"\$@\""
first_run_pending "现场二/首次"
rebind_loop
OUT=$($BFK resize "$IMG:1" 56M --grow-lv --lv lv1 2>&1); E=$?
echo "$OUT" | sed 's/^/    /'
exp "$E" 5 "LV 已扩但核验失败 → 部分完成（5）"
case "$OUT" in *"simulated verification failure"*) echo "  OK   核验失败原因随待办报出";; *) bad "未报出核验失败原因";; esac
expect_record "现场二/注入"
report_state "$((LV0 + 8388608))" "现场二/注入"   # LV 已到目标
OUT=$($BFK resize "$IMG:1" 56M --grow-lv --lv lv1 2>&1); E=$?
echo "$OUT" | sed 's/^/    /'
exp "$E" 0 "重跑确认已在目标、不重复扩"
report_state "$((LV0 + 8388608))" "现场二/重跑"
expect_no_record "现场二/重跑"

echo "-- 现场三：已提交的 PV 作业用 grow 重跑 --"
# grow 的右边界由当前空闲现算，而作业记录的目标是 56M。分派层若按"右侧有空闲"另判成
# 一次新作业，就会把这份已提交的现场判成"不是记录里那次请求"并拒绝，LVM 收尾永远做不完
build_fixture
first_run_pending "现场三/首次"
rebind_loop
OUT=$($B resize "$IMG:1" grow --grow-lv --lv lv1 2>&1); E=$?
echo "$OUT" | sed 's/^/    /'
exp "$E" 0 "grow 重跑按记录的目标续做 LVM 收尾"
report_state "$((LV0 + 8388608))" "现场三/重跑"
expect_no_record "现场三/重跑"

vgchange -an "$VG" >/dev/null 2>&1
if [ -n "$LOOP" ]; then lo_detach "$LOOP"; LOOP=; fi
vgremove -ff "$VG" >/dev/null 2>&1
rm -rf "$FAKE_TOOL_DIR"
rm -rf "$MARK"
exit $rc
