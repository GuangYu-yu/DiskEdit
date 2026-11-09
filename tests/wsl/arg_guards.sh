#!/usr/bin/env bash
# 参数层守卫（不需挂载，快速）：未登记旗标 / --chunk 取值域 / --align 取值 / 语义与互斥拒绝。
# 退出码契约唯一定义在 src/outcome.rs:20-32（0 完成 / 2 参数或命令语法错误 /
# 3 请求与现状不匹配或前置条件不满足 / 4 未写盘的环境故障 / 5 部分完成 / 6 平台或能力不支持）；
# 参数缺值/坏值经 bad_arg 走 Fail::usage（src/args.rs:246-248），因此都是 2。
# 每条断言的依据以 `src/文件:行号` 标注在断言旁。
source "$(dirname "$0")/lib.sh"
require_bin

img T arg_guards.img

truncate -s 32M "$T"
$B new "$T" --yes >/dev/null 2>&1; exp $? 0 "new（32MiB GPT）"
$B add "$T" 1M --align mib --name p1 --yes >/dev/null 2>&1; exp $? 0 "add 1MiB p1"

echo "== --chunk-size 未登记 / --chunk 取值域 =="
# `--chunk-size` 不在完整注册表内（「未登记（收到即 2）」）；未声明选项 → 2。
# `--chunk` 登记给 move/copy/resize，取值 1..=64 MiB。
# 合法边界要真的走一次 chunk 路径才有意义：copy 由 cli.rs 证过会消费 chunk，源分区仅 1MiB，
# 两次拷贝代价极小（chunk 大于数据时即单块），断言后立刻 delete 掉落点。
$B copy "$T":1 --start 4096 --chunk-size 1 --yes >/dev/null 2>&1; exp $? 2 "--chunk-size 未登记（2）" # cli-surface: refused
$B copy "$T":1 --start 4096 --chunk 1 --yes >/dev/null 2>&1; exp $? 0 "--chunk 1 被接受（下界）"
$B delete "$T":2 --yes >/dev/null 2>&1; exp $? 0 "  清理 copy 落点 #2"
$B copy "$T":1 --start 4096 --chunk 64 --yes >/dev/null 2>&1; exp $? 0 "--chunk 64 被接受（上界）"
$B delete "$T":2 --yes >/dev/null 2>&1; exp $? 0 "  清理 copy 落点 #2"
$B copy "$T":1 --start 4096 --chunk 0 --yes >/dev/null 2>&1; exp $? 2 "--chunk 0 越界拒绝"
$B copy "$T":1 --start 4096 --chunk 65 --yes >/dev/null 2>&1; exp $? 2 "--chunk 65 越界拒绝"

echo "== --align 取值 =="
# src/support.rs:262-271（align_unit）：mib|none 之外的值 usage（2）；
# src/cmd/mod.rs:298-306：add 声明消费 --align，故 add 能把请求送到 align_unit
# add 的终点由 SIZE 定：2048 扇区 × 512 = 1048576 字节（8192..10239 / 12288..14335 / 49152..51199）
$B add --yes "$T" 1048576 --start 8192 --align mib >/dev/null 2>&1; exp $? 0 "--align mib 被接受"
$B add --yes "$T" 1048576 --start 12288 --align none >/dev/null 2>&1; exp $? 0 "--align none 被接受"
$B add --yes "$T" 1048576 --start 49152 --align banana >/dev/null 2>&1; exp $? 2 "--align banana 非法值拒绝"

echo "== 语义 / 互斥拒绝 =="
# src/cmd/mod.rs:111-114 + src/args.rs:118-125：SIZE 只有位置参数一种给法，--size 不在 resize 的
# 白名单里，给了即 usage（2）
$B resize "$T":1 --size 8M 16M >/dev/null 2>&1; exp $? 2 "resize --size 非本命令选项（2）" # cli-surface: refused
# resize-part 不接受 --start（未登记），起点取当前表项；
# 该夹具里 p1 起点 2048、末端 4095
# --end 等于当前末端即空操作，按成功报 0
$B resize-part --yes "$T":1 --end 4095 >/dev/null 2>&1; exp $? 0 "resize-part --end 当前末端（空操作，0）"

echo
echo "==== 参数守卫结束（rc=$rc）===="
exit $rc