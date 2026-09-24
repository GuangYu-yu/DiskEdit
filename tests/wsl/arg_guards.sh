#!/usr/bin/env bash
# 参数层守卫（不需挂载，快速）：--chunk-size 取值域 / --align 取值 / 语义与互斥拒绝。
# 退出码契约唯一定义在 src/outcome.rs:17-25（0 完成 / 10 拒绝=未写盘且请求与现状不匹配 /
# 20 部分完成 / 30 基础设施失败）；参数缺值/坏值经 bad_arg 走 Fail::refused（src/args.rs:246-248）
# 因此都是 10。每条断言的依据以 `src/文件:行号` 标注在断言旁。
source "$(dirname "$0")/lib.sh"
require_bin

T=/var/tmp/arg_guards.img
track_file "$T"

rm -f "$T" "$T$JOURNAL_SUFFIX"
truncate -s 32M "$T"
$B new "$T" --yes >/dev/null 2>&1; exp $? 0 "new（32MiB GPT）"
$B create "$T" --size 1M --name p1 >/dev/null 2>&1; exp $? 0 "create 1MiB p1"

echo "== --chunk-size 取值域 =="
# src/args.rs:184-193：取值必须在 1..=1024 MiB，越界当场 bad_arg → refused（10）。
# 合法边界要真的走一次 chunk 路径才有意义：copy 由 cli.rs 证过会消费 chunk，源分区仅 1MiB，
# 两次拷贝代价极小（chunk 大于数据时即单块），断言后立刻 del 掉落点。
$B copy "$T":1 --start 4096 --chunk-size 1 >/dev/null 2>&1; exp $? 0 "--chunk-size 1 被接受（下界）"
$B del "$T":2 --yes >/dev/null 2>&1; exp $? 0 "  清理 copy 落点 #2"
$B copy "$T":1 --start 4096 --chunk-size 1024 >/dev/null 2>&1; exp $? 0 "--chunk-size 1024 被接受（上界）"
$B del "$T":2 --yes >/dev/null 2>&1; exp $? 0 "  清理 copy 落点 #2"
$B copy "$T":1 --start 4096 --chunk-size 0 >/dev/null 2>&1; exp $? 10 "--chunk-size 0 越界拒绝"
$B copy "$T":1 --start 4096 --chunk-size 1025 >/dev/null 2>&1; exp $? 10 "--chunk-size 1025 越界拒绝"

echo "== --align 取值 =="
# src/support.rs:218-224（align_unit）：mib|cyl|none 之外的值 refused（10）；
# src/cmd/mod.rs:170：add 声明消费 --align，故 add 能把请求送到 align_unit
$B add "$T" --start 8192 --end 10239 --align mib >/dev/null 2>&1; exp $? 0 "--align mib 被接受"
$B add "$T" --start 12288 --end 14335 --align none >/dev/null 2>&1; exp $? 0 "--align none 被接受"
# cyl = 16065 扇区/柱面（src/support.rs:222）：17000..49000 上取整/下取整为 32130..48194，
# 落在 32MiB 镜像的 last_usable 内且不与上面两条重叠 ⇒ 0
$B add "$T" --start 17000 --end 49000 --align cyl >/dev/null 2>&1; exp $? 0 "--align cyl 被接受"
$B add "$T" --start 49152 --end 51199 --align banana >/dev/null 2>&1; exp $? 10 "--align banana 非法值拒绝"

echo "== 语义 / 互斥拒绝 =="
# src/cmd/resize.rs:275-277：--size 与 --grow-to-end 是两种给终点的方式，同给即 refused（10）
$B resize "$T":1 --size 8M --grow-to-end >/dev/null 2>&1; exp $? 10 "resize --size + --grow-to-end 互斥拒绝"
# src/cmd/layout.rs:164：resize-part 必须同时给出 :N 与 --start，缺 --start 落到 usage()
# → src/args.rs:51-54 以 EXIT_REFUSED（src/outcome.rs:20）退出，即 10
$B resize-part "$T":1 --end 4095 >/dev/null 2>&1; exp $? 10 "resize-part 缺 --start 用法错误（10）"

echo
echo "==== 参数守卫结束（rc=$rc）===="
exit $rc