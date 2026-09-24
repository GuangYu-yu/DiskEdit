#!/usr/bin/env bash
# 全命令冒烟：每一步都断言退出码。
# 契约见 src/outcome.rs：0 完成 / 10 拒绝（未写盘，请求与现状不匹配）/ 20 部分完成 / 30 基础设施
source "$(dirname "$0")/lib.sh"
require_bin

# 额外收尾：清掉 ckpt 状态目录（文件镜像的 journal 随 track_file 一并删除）
cleanup_hook() { rm -f /var/lib/diskedit/* 2>/dev/null; }

T=/var/tmp/diskedit_test4.img
track_file "$T"
rm -f "$T" "$T"$SIDECAR_GLOB 2>/dev/null

truncate -s 64M "$T"

echo "== new（建表）=="
$B new "$T" --yes; exp "$?" 0 "new" || exit 1

echo "== create p1 (32M bytes, ext4) =="
$B create "$T" --size 33554432 --name test1 --fs ext4; exp "$?" 0 "create p1 + mkfs.ext4"

echo "== create p2 (16M bytes) =="
$B create "$T" --size 16777216 --name test2; exp "$?" 0 "create p2（无 FS）"

echo "== info =="
INFO=$($B info "$T"); E=$?
echo "$INFO"; exp "$E" 0 "info"

echo "== check =="
$B check "$T":1; exp "$?" 0 "check p1"

echo "== set name =="
$B set "$T":2 name renamed2; exp "$?" 0 "set name"

echo "== resize grow +8M =="
$B resize "$T":2 +8M; exp "$?" 0 "resize +8M"

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
$B set "$T":2 label mydata; exp "$?" 0 "set label"

echo "== resize shrink -4M（带 FS ⇒ 先缩 FS 再收表，故不是 --no-fs 的 10）=="
$B resize "$T":2 -4M; exp "$?" 0 "resize -4M（先缩 FS）"

echo "== move p2 =="
$B move "$T":2 --start end; exp "$?" 0 "move p2"

echo "== info final =="
FINAL=$($B info "$T"); E=$?
echo "$FINAL"; exp "$E" 0 "info"

echo "== undo（上一步已成功 ⇒ journal 已 drop，无凭据可回滚）=="
OUT=$($B undo "$T" --yes 2>&1); E=$?
echo "$OUT" | sed 's/^/    /'
exp "$E" 10 "undo 拒绝：无 journal（10 = 未写盘）"
echo "$OUT" | grep -q "no undo journal" || { echo "  BAD  undo 的拒绝理由不是\"无 journal\""; rc=1; }

echo "== info after undo（拒绝 ⇒ 布局一字未改）=="
AFTER=$($B info "$T"); E=$?
echo "$AFTER"; exp "$E" 0 "info after undo"
[ "$AFTER" = "$FINAL" ] && echo "  OK   undo 被拒后布局未变" \
  || { echo "  BAD  undo 被拒却改了布局"; rc=1; }

echo
echo "==== 汇总：$( [ "$rc" = 0 ] && echo 全部符合契约 || echo 有断言失败 ) ===="
exit $rc