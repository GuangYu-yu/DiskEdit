#!/usr/bin/env bash
# 在线 btrfs 的挂载身份（src/online/target.rs）：
# mountinfo 字段 3 对 btrfs 是匿名设备号，身份改由 FSID → /sys/fs/btrfs/<fsid>/devices/
# → 该条目的 dev 属性 → 与节点 st_rdev 比对。本脚本守三件事：
#   A 单设备 btrfs 在线扩缩容仍可用（该判据曾把整条路径拒掉，须有回归）
#   B 路径不变而节点指向另一设备时拒绝，且不写盘
#   C 多设备 btrfs 在线拒绝
source "$(dirname "$0")/lib.sh"
require_bin

echo "########## A: 单设备 btrfs 在线 +16M / -16M ##########"
img T t21.img
rm -rf "$T" "$T"$SIDECAR_GLOB 2>/dev/null
truncate -s 1G "$T"
$B new "$T" --yes >/dev/null
# 400M 而非 200M：btrfs 的 resize 下限是 256MiB，更小的文件系统无论扩缩都被内核
# 以 EINVAL 拒绝（实测 btrfs filesystem resize 报 "unable to resize: Invalid argument"）
$B add "$T" 400M --align mib --name bt --yes >/dev/null; echo "add exit=$?"
$B mkfs "$T":1 btrfs --yes; echo "mkfs exit=$?"
LD=$(lo_attach "$T")
lo_waitpart "${LD}p1" || { echo "SKIP: ${LD}p1 未就绪"; exit 0; }
P=${LD}p1
mount_at "$P" "$TEST_MNT" || { echo "SKIP: 无法挂载 $P"; exit 0; }
S0=$(blockdev --getsize64 "$P")
D0=$(df -B1 --output=size "$TEST_MNT" | tail -1 | tr -d ' ')
OUT=$($B resize --yes "${LD}:1" +16M 2>&1); E=$?
echo "grow exit=$E : $(echo "$OUT" | tail -1)"
exp "$E" 0 "A/grow"
S1=$(blockdev --getsize64 "$P")
D1=$(df -B1 --output=size "$TEST_MNT" | tail -1 | tr -d ' ')
assert_eq "$S1" "$((S0 + 16777216))" "A/分区 +16M"
if [ "$D1" -gt "$D0" ]; then echo "  OK   A/fs 同步变大 ($D0 → $D1)"; else echo "  BAD  A/fs 未变大 ($D0 → $D1)"; rc=1; fi
# 缩容的唯一入口是 resize-part（离线：临时挂载缩 FS、再收表，同一份记录）；
# resizefs 挂载点形态不改表、一律拒绝缩容
unmount_now "$TEST_MNT"
OUT=$($B resize-part --yes "$T":1 --end 821247 2>&1); E=$?
echo "shrink exit=$E : $(echo "$OUT" | tail -1)"
exp "$E" 0 "A/resize-part 缩回原尺寸"
assert_eq "$(blockdev --getsize64 "$P")" "$S0" "A/回到原尺寸"

echo
echo "########## B: 节点重绑（路径不变、设备号变了）必须拒绝 ##########"
# 手法：私有 mount namespace 内把伪造的设备节点 bind 到挂载所用的那条路径，
# 真实 /dev 不动，随 unshare 退出消失。伪造节点取自另一台设备（整盘 loop）。
# 入口用 resizefs 的挂载点形式：btrfs 身份判据在从挂载点解析目标时执行，
# 以块设备为目标的入口会先以 O_EXCL 打开设备，已挂载的设备在那里就是 EBUSY。
img FAKE t21f.img
FK=$WSL_ROOT/t21fake
truncate -s 64M "$FAKE"
LF=$(lo_attach "$FAKE")
mkdir -p "$FK"
read -r MJ MN < <(stat -c '%t %T' "$LF")
mknod "$FK/dev" b "$((16#$MJ))" "$((16#$MN))"
mount_at "$P" "$TEST_MNT"
S0=$(blockdev --getsize64 "$P")
if unshare -m --propagation private true 2>/dev/null; then
  OUT=$(unshare -m --propagation private bash -c \
    "mount --bind '$FK/dev' '$P' && exec '$B' resizefs --yes "$TEST_MNT" 400M" 2>&1); E=$?
  echo "rebound exit=$E : $(echo "$OUT" | tail -1)"
  exp "$E" 3 "B/重绑拒绝"
  if echo "$OUT" | grep -q "but the node now resolves to"; then
    echo "  OK   B/报身份不一致"
  else
    echo "  BAD  B/未报身份不一致"; rc=1
  fi
  assert_eq "$(blockdev --getsize64 "$P")" "$S0" "B/未写盘"
else
  echo "SKIP: unshare -m 不可用，B 段未执行"
fi
unmount_now "$TEST_MNT"
rm -rf "$FK"

echo
echo "########## C: 多设备 btrfs 在线拒绝 ##########"
img T2 t21m.img
img T3 t21m2.img
track_file "$T2"; rm -rf "$T2" "$T2"$SIDECAR_GLOB "$T3" "$T3"$SIDECAR_GLOB 2>/dev/null
truncate -s 1G "$T2"
$B new "$T2" --yes >/dev/null
$B add --yes "$T2" 533725184 --start 2048 --name btm >/dev/null
truncate -s 512M "$T3"
L2=$(lo_attach "$T2")
L3=$(lo_attach "$T3")
mkfs.btrfs -f -d single -m single "${L2}p1" "$L3" >/dev/null 2>&1; echo "mkfs.btrfs(2 dev) exit=$?"
lo_waitpart "${L2}p1" || { echo "SKIP: ${L2}p1 未就绪"; exit 0; }
mount_at "${L2}p1" "$TEST_MNT"
S0=$(blockdev --getsize64 "${L2}p1")
OUT=$($B resize --yes "${L2}:1" +16M 2>&1); E=$?
echo "multidev exit=$E : $(echo "$OUT" | tail -1)"
if [ "$E" = 6 ] && echo "$OUT" | grep -q "has 2 devices"; then
  echo "  OK   C/多设备拒绝"
else
  echo "  BAD  C/多设备拒绝 (exit=$E, want 6 + 'has 2 devices')"; rc=1
fi
assert_eq "$(blockdev --getsize64 "${L2}p1")" "$S0" "C/未写盘"
unmount_now "$TEST_MNT"
exit $rc
