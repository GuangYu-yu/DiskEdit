#!/usr/bin/env bash
# 扩容/搬移/resizefs 链路：普通扩容、--allow-move、grow 尾打包，FS 三种 resizefs 调用，
# 每步断言退出码（契约见 src/outcome.rs）并断言 marker 存活，终态 e2fsck + sgdisk
source "$(dirname "$0")/lib.sh"
require_bin

T=/var/tmp/diskedit_test5.img
MNT=/testmnt

cleanup_hook() { rm -f /var/lib/diskedit/* 2>/dev/null; }

rm -f "$T" "$T".diskedit.* 2>/dev/null
rm -f /var/lib/diskedit/* 2>/dev/null
mkdir -p "$MNT"
track_file "$T"

# 表变更后内核分区视图必须刷新：每次重新 attach loop
verify_marker() { # label
  local out
  LD=$(lo_attach "$T")
  if mount_at "${LD}p1" "$MNT"; then
    out=$(cat "$MNT/marker.txt" 2>/dev/null)
    umount "$MNT"
    [ "$out" = "hello-diskedit" ] \
      && echo "  OK   marker 存活 [$1]" \
      || { echo "  BAD  marker 丢失/损坏 [$1] (got [$out])"; rc=1; }
  else
    echo "  BAD  marker 分区挂不上 [$1]"; rc=1
  fi
  lo_detach "$LD"
}

truncate -s 64M "$T"
$B new "$T" --yes >/dev/null; exp "$?" 0 "new"
$B create "$T" --size 33554432 --name test1 --fs ext4 >/dev/null
exp "$?" 0 "create p1（32MiB ext4）"

echo "== sgdisk verify (backup header state) =="
sgdisk -v "$T" >/dev/null 2>&1; exp "$?" 0 "sgdisk -v（表自洽）"

echo "== fill marker =="
LD=$(lo_attach "$T")
mount_at "${LD}p1" "$MNT" || { echo "  BAD  首次挂载失败"; lo_detach "$LD"; exit 1; }
echo "hello-diskedit" > "$MNT/marker.txt"
umount "$MNT"
lo_detach "$LD"
echo "  OK   marker 已写入"

echo "== resize +8M (plain) =="
$B resize "$T":1 +8M >/dev/null; exp "$?" 0 "resize +8M"
verify_marker "resize +8M"
$B check "$T":1 >/dev/null; exp "$?" 0 "check p1"

echo "== resize +8M --allow-move (data survives move) =="
$B resize "$T":1 +8M --allow-move --yes >/dev/null; exp "$?" 0 "resize +8M --allow-move"
verify_marker "resize +8M --allow-move"

echo "== resize grow --allow-move --no-fs (tail packing) =="
# --no-fs ⇒ 只要布局成功就是 0（FS 步骤被显式排除在契约外），故此处 0 是在断言
# "分区已吃满尾段"而非"FS 也扩了"；下一段 resizefs 才补 FS
$B resize "$T":1 grow --allow-move --yes --no-fs >/dev/null; exp "$?" 0 "resize grow --no-fs"
$B info "$T" | grep -q '"num":1,"first_lba":2048,"last_lba":131038,' \
  && echo "  OK   分区已到 last_usable(131038)" || { echo "  BAD  尾打包未到位"; rc=1; }
verify_marker "resize grow --no-fs"

echo "== resize-part --grow-to-end（此刻分区终点已是 last_usable，故无实际变更）=="
# resize-part 与 resize 同契约：默认连 FS 一起扩，--no-fs 才只动表
$B resize-part "$T":1 --start 2048 --grow-to-end >/dev/null; exp "$?" 0 "resize-part --grow-to-end"

echo "== resizefs <mountpoint> <BYTES> --online（BYTES = 分区目标大小）=="
# 上一步 --no-fs 把 FS 留在了 42MiB（分区终点 100351，48MiB），而分区现在已是
# 63MiB：在线扩后 FS 应长到 ~57MiB，故 50MiB 是"确实扩了"的判据
SZ=$($B info "$T" | grep -o '"num":1,[^}]*' | grep -o '"size_bytes":[0-9]*' | grep -o '[0-9]*')
echo "partition size_bytes=$SZ"
LD=$(lo_attach "$T")
mount_at "${LD}p1" "$MNT" || { echo "  BAD  --online 前挂载失败"; lo_detach "$LD"; exit 1; }
FS_BEFORE=$(df -B1M --output=size "$MNT" | tail -1 | tr -d ' ')
$B resizefs "$MNT" "$SZ" --online; exp "$?" 0 "resizefs --online"
FS_AFTER=$(df -B1M --output=size "$MNT" | tail -1 | tr -d ' ')
echo "  FS size: ${FS_BEFORE}MiB -> ${FS_AFTER}MiB"
[ "$FS_AFTER" -ge 50 ] && [ "$FS_AFTER" -ge "$FS_BEFORE" ] \
  && echo "  OK   FS 已扩到填充分区" \
  || { echo "  BAD  FS 未扩（${FS_BEFORE} -> ${FS_AFTER}）"; rc=1; }
[ "$(cat "$MNT/marker.txt")" = "hello-diskedit" ] \
  && echo "  OK   marker still: hello-diskedit" \
  || { echo "  BAD  在线扩后 marker 丢失"; rc=1; }
umount "$MNT"
lo_detach "$LD"

echo "== resizefs <T>:N offline (grow into partition) =="
$B resizefs "$T":1 >/dev/null; exp "$?" 0 "resizefs 离线"
LD=$(lo_attach "$T")
if mount_at "${LD}p1" "$MNT"; then
  FS_MB=$(df -B1M --output=size "$MNT" | tail -1 | tr -d ' ')
  echo "  FS size: ${FS_MB}MiB"
  [ "$FS_MB" -ge 50 ] && echo "  OK   离线 resizefs 后 FS 仍填充分区" \
    || { echo "  BAD  离线 resizefs 后 FS 反而变小（${FS_MB}MiB）"; rc=1; }
  umount "$MNT"
else
  echo "  BAD  离线 resizefs 后挂不上"; rc=1
fi
lo_detach "$LD"

echo "== final: e2fsck + sgdisk =="
LD=$(lo_attach "$T")
# 不用 `cmd | tail -1`：那样 $? 是 tail 的，e2fsck 自己的退出码会被吞掉
E2OUT=$(e2fsck -f -y "${LD}p1" 2>&1); E=$?
echo "  $(echo "$E2OUT" | tail -1)"
lo_detach "$LD"
exp "$E" 0 "e2fsck -f -y"
sgdisk -v "$T" >/dev/null 2>&1; exp "$?" 0 "sgdisk -v（终态自洽）"

echo
echo "==== 汇总：$( [ "$rc" = 0 ] && echo 全部符合契约 || echo 有断言失败 ) ===="
exit $rc