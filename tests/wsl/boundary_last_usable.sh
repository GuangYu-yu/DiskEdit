#!/usr/bin/env bash
# last_usable 边界：1GiB GPT 的可用区为 34..2097118
# 判定：逐条断言退出码；越界请求必须是 10（拒绝），且拒绝不得改动表
source "$(dirname "$0")/lib.sh"
require_bin

T=/var/tmp/t28.img
LU=2097118   # 1GiB/512 = 2097152 扇区；去掉备份数组(32)与备份头(1)后的末个可用 LBA
track_file "$T"

p1_last() { # p1 的 last_lba
  $B info "$T" | grep -o '"num":1,[^}]*}' | grep -o '"last_lba":[0-9]*' | cut -d: -f2
}

rm -f "$T" "$T".diskedit.* 2>/dev/null
truncate -s 1G "$T"

echo "== new（1GiB）=="
$B new "$T" --yes >/dev/null; exp $? 0 "new"

echo "== add p1（2048..4095）=="
$B add "$T" --start 2048 --end 4095 --name p1 >/dev/null; exp $? 0 "add p1（2048..4095）"

echo "== 扩到 last_usable =="
$B resize-part "$T":1 --start 2048 --grow-to-end >/dev/null; exp $? 0 "at last_usable"
[ "$(p1_last)" = "$LU" ] && echo "  OK   p1 已到 2048..$LU" \
  || { echo "  BAD  p1 未到 last_usable（last_lba=$(p1_last)）"; rc=1; }

echo "== 越过 last_usable 必须拒绝 =="
# 三条都必须显式 --align none。--end 默认按 MiB **下取整**（src/support.rs:231,235 的
# align_range），会把越界的 end 夹回可用区内（2097119 → 2095103、2097150 → 2095103）；
# 这样得到的仍是可用区内的合法区间，被拒的理由变成"缩容 unknown FS"或"与既有分区重叠"，
# 于是断言虽拿到 10，却根本没走到边界校验——即"因错因通过"。
# 加 --align none 后原始越界 LBA 直达校验：resize-part 命中 src/movepart.rs:1456 的
# `new_end > last_usable_lba`，add 命中 src/gpt_policy.rs:290 的可用区判定，拒绝才是因越界。
# 反例佐证：uncovered_branches.sh 的紧贴边界用例同样使用 --align none。
$B resize-part "$T":1 --start 2048 --align none --end $((LU + 1)) >/dev/null 2>&1; exp $? 10 "last_usable+1 拒绝"
$B add "$T" --start 2048 --align none --end 2097150 >/dev/null 2>&1; exp $? 10 "beyond last_usable 拒绝"
$B add "$T" --start $((LU + 1)) --align none --end 2097150 >/dev/null 2>&1; exp $? 10 "start 越界拒绝"

echo "== 拒绝不得改动表 =="
[ "$(p1_last)" = "$LU" ] && echo "  OK   拒绝未改动表" \
  || { echo "  BAD  拒绝路径改动了表"; rc=1; }

echo
echo "==== last_usable 边界结束（rc=$rc）===="
exit $rc