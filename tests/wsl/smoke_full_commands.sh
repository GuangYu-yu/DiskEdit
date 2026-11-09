#!/usr/bin/env bash
# 全命令冒烟：每一步都断言退出码。
# 契约见 src/outcome.rs：0 完成 / 2 参数或命令语法错误 / 3 拒绝（未写盘，请求与现状不匹配）/
# 4 未写盘的环境故障 / 5 部分完成 / 6 平台或能力不支持
source "$(dirname "$0")/lib.sh"
require_bin



img T smoke4.img
rm -rf "$T" "$T"$SIDECAR_GLOB 2>/dev/null

truncate -s 64M "$T"

echo "== new（建表）=="
$B new "$T" --yes; exp "$?" 0 "new" || exit 1

echo "== add p1 (32M bytes) + mkfs ext4 =="
$B add "$T" 33554432 --align mib --name test1 --yes; exp "$?" 0 "add p1"
$B mkfs "$T":1 ext4 --yes; exp "$?" 0 "mkfs p1 + mkfs.ext4"

echo "== add p2 (16M bytes) =="
$B add "$T" 16777216 --align mib --name test2 --yes; exp "$?" 0 "add p2（无 FS）"

echo "== info =="
INFO=$($B info "$T"); E=$?
echo "$INFO"; exp "$E" 0 "info"

echo "== check =="
$B check "$T":1; exp "$?" 0 "check p1"

echo "== set name =="
$B set --yes "$T":2 name renamed2; exp "$?" 0 "set name"

echo "== resize grow +8M =="
# p2 这时还没有 FS（下面的 mkfs 才建）：扩分区表要显式 --no-fs，否则写表之前就拒绝
$B resize --yes "$T":2 +8M --no-fs; exp "$?" 0 "resize +8M"

echo "== info（p2 应已到 67584..116735）=="
INFO=$($B info "$T"); E=$?
echo "$INFO"; exp "$E" 0 "info"
echo "$INFO" | grep -q '"num":2,"first_lba":67584,"last_lba":116735' \
  && echo "  OK   p2 布局符合预期" || { echo "  BAD  p2 布局不符（+8M 未生效？）"; rc=1; }

echo "== mkfs ext4 on p2 =="
$B mkfs "$T":2 ext4 --yes; exp "$?" 0 "mkfs p2"

echo "== check p2 =="
$B check "$T":2; exp "$?" 0 "check p2"

echo "== set label =="
$B set --yes "$T":2 label mydata; exp "$?" 0 "set label"

echo "== resize shrink -4M（resize-part 一条命令：先离线缩 FS，再收表）=="
# p2 现为 67584..116735（24MiB），缩到 20MiB ⇒ last_lba = 67584+40960-1 = 108543
$B resize-part --yes "$T":2 --end 108543; exp "$?" 0 "resize-part 缩到 20M"

echo "== move p2 =="
$B move "$T":2 --start end --yes; exp "$?" 0 "move p2"

echo "== info final =="
FINAL=$($B info "$T"); E=$?
echo "$FINAL"; exp "$E" 0 "info"

echo "== undo（上一步已成功 ⇒ 记录已释放，无凭据可回滚）=="
OUT=$($B undo "$T" --yes 2>&1); E=$?
echo "$OUT" | sed 's/^/    /'
exp "$E" 3 "undo 拒绝：无记录（3 = 未写盘）"
echo "$OUT" | grep -q "no recovery record on" || { echo "  BAD  undo 的拒绝理由不是\"无记录\""; rc=1; }

echo "== info after undo（拒绝 ⇒ 布局一字未改）=="
AFTER=$($B info "$T"); E=$?
echo "$AFTER"; exp "$E" 0 "info after undo"
[ "$AFTER" = "$FINAL" ] && echo "  OK   undo 被拒后布局未变" \
  || { echo "  BAD  undo 被拒却改了布局"; rc=1; }

echo
echo "==== 汇总：$( [ "$rc" = 0 ] && echo 全部符合契约 || echo 有断言失败 ) ===="
exit $rc