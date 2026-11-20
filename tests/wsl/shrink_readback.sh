#!/usr/bin/env bash
# 缩容后回读**实际落点**：工具成功返回只说明它没报错，不说明 FS 落在请求的尺寸上。
# 覆盖四条：正常回读（请求值 = 实际落点）、工具向下取整（实际 < 请求值）、
# 回读到的落点大于请求值（工具行为不符合预期，分区不缩）、回读拿不到落点
# （缩容可能已完成但落点无法证实：表不写、分区保持原尺寸，落一条待办而不是报失败）。
# 入口是 resize --end；SIZE 的收缩形式（`-100M`）共用同一段机制。MBR 段钉的是
# 主分区走同一条“先缩 FS、再收条目”的路径，并在表写之后中断，核对已提交记录的续跑：
# 同一请求按记录收尾、盘上区间被外部改过则拒绝。
#
# 假 dumpe2fs 靠工具表变体生效：工具解析在编译期钉死（src/fsops/env.rs 的 TOOL_PATH），
# PATH 影子不参与。BFK 的表把假目录排在系统目录之前，其余工具仍落到真身。
source "$(dirname "$0")/lib.sh"
require_fake_bin
require_fault_bin

img T t42.img

FAKE=$FAKE_TOOL_DIR
cleanup_hook() { rm -rf "$FAKE" 2>/dev/null; }

# 假工具：只覆盖回读要用的那一个可执行文件，其余仍走工具表里的系统目录
fake() { # $1=名  $2=脚本体
    printf '%s\n' "$2" > "$FAKE/$1"
    chmod +x "$FAKE/$1"
}

setup_gpt() {
    rm_img "$T"
    truncate -s 512M "$T"
    $B new "$T" --yes >/dev/null
    $B add "$T" 400M --align mib --name p1 --yes >/dev/null 2>&1
    $B mkfs "$T":1 ext4 --yes >/dev/null 2>&1
}

# 分区 1 的字节数（info 输出里盘与分区各有一处 size_bytes，取分区那条）
pt1() { $B info "$T" | grep -o '"num":1,[^}]*}' | grep -o '"size_bytes":[0-9]*'; }

echo "===== A: ext 缩容，回读正常 → 成功行同时给请求值与实际落点 → 0 ====="
setup_gpt
# 400M（2048..821247）缩到 300M：新末端 = 2048+614400-1 = 616447
OUT=$($B resize "$T":1 --end 616447 --yes 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | sed 's/^/    /'
exp "$E" 0 "落点在请求范围内即完整成功"
case "$OUT" in
    *"fs shrunk to 314572800 bytes (requested 314572800)"*) echo "A both sizes OK";;
    *) echo "A size line missing (BAD)"; rc=1;;
esac

echo
echo "===== B: 请求值不是 FS 块大小的整数倍 → 工具向下取整，实际 < 请求 → 0 ====="
setup_gpt
REQ=104858112   # 100MiB + 512B：512 的整数倍（204801 扇区）、4096 的非整数倍（25600.125 块）
# 204801 扇区 ⇒ 新末端 = 2048+204801-1 = 206848
OUT=$($B resize "$T":1 --end 206848 --yes 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | grep -E 'fs shrunk|refused|error' | sed 's/^/    /'
exp "$E" 0 "取整落点仍在请求范围内即成功"
case "$OUT" in
    *"fs shrunk to 104857600 bytes (requested 104858112)"*) echo "B rounding visible OK";;
    *) echo "B rounding not reported as expected (BAD)"; rc=1;;
esac

echo
echo "===== C: 回读落点大于请求值 → 工具行为不符合预期，分区不缩 → 5 ====="
setup_gpt
BEFORE=$(pt1)
rm -rf "$FAKE"; mkdir -p "$FAKE"
# Block size 保持真值：缩容前的 min-size 预查（resize2fs -P × dumpe2fs 块大小）要用它；
# 回读取的 Block count 故意报一个大数
fake dumpe2fs '#!/bin/sh
echo "Block count:              999999999"
echo "Block size:               4096"
echo "Blocks per group:         32768"'
OUT=$($BFK resize "$T":1 --end 616447 --yes 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | sed 's/^/    /'
exp "$E" 5 "落点超出请求值不是成功（可能已写盘，带警示）"
case "$OUT" in
    *4095999995904*) echo "C reported actual size OK;";;
    *) echo "C actual size missing (BAD)"; rc=1;;
esac
case "$OUT" in
    *314572800*) echo "C reported requested size OK";;
    *) echo "C requested size missing (BAD)"; rc=1;;
esac
assert_eq "$(pt1)" "$BEFORE" "C 拒绝必须发生在写表之前（分区尺寸未变）"

echo
echo "===== D: 回读拿不到落点 → 落一条 fs shrink 待办，表不写、分区保持原尺寸 → 5 ====="
setup_gpt
rm -rf "$FAKE"; mkdir -p "$FAKE"
# 只给 Block size（预查要用），回读要的 Block count 缺失 = 回读拿不到落点
fake dumpe2fs '#!/bin/sh
echo "Block size:               4096"'
OUT=$($BFK resize "$T":1 --end 616447 --yes 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | sed 's/^/    /'
exp "$E" 5 "回读不出来不是缩容失败，改出一条待办"
case "$OUT" in *"fs shrink"*) echo "D pending labelled OK";; *) echo "D pending missing (BAD)"; rc=1;; esac
case "$OUT" in *"could not be read back"*) echo "D unverifiable wording OK";; *) echo "D wording missing (BAD)"; rc=1;; esac
case "$OUT" in
    *"run manually: dumpe2fs -h <partition device>"*) echo "D remedy OK";;
    *) echo "D remedy missing (BAD)"; rc=1;;
esac
assert_eq "$(pt1)" '"size_bytes":419430400' "D 落点未证实不写表（分区保持原尺寸）"

echo
echo "===== E: MBR 主分区同样可缩（先离线缩 FS，再收条目） ====="
rm_img "$T"
truncate -s 512M "$T"
$B new "$T" --table mbr --yes >/dev/null
$B add "$T" 400M --align mib --yes >/dev/null 2>&1
$B mkfs "$T":1 ext4 --yes >/dev/null 2>&1
OUT=$($B resize "$T":1 -100M --yes 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | grep -E 'fs shrunk|refused|error' | sed 's/^/    /'
exp "$E" 0 "MBR 主分区走同一条离线收缩路径"
case "$OUT" in
    *"fs shrunk to 314572800 bytes"*) echo "E shrink landing OK";;
    *) echo "E shrink landing missing (BAD)"; rc=1;;
esac
assert_eq "$(pt1)" '"size_bytes":314572800' "E 400M - 100M = 300M"

echo
echo "===== F: 预查命令把块大小报成 0 → 写盘前拒绝，表不变 → 4 ====="
setup_gpt
BEFORE=$(pt1)
rm -rf "$FAKE"; mkdir -p "$FAKE"
# 预查要的 Block size 报 0（0 不是可用尺寸）；Block count 一并给出，贴近真实输出
fake dumpe2fs '#!/bin/sh
echo "Block count:              8192"
echo "Block size:               0"'
OUT=$($BFK resize "$T":1 --end 616447 --yes 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | sed 's/^/    /'
exp "$E" 4 "0 不是可用尺寸，预查拒绝"
case "$OUT" in
    *"not a usable size"*) echo "F unusable-value reason OK";;
    *) echo "F reason missing (BAD)"; rc=1;;
esac
case "$OUT" in
    *"fs shrunk"*) echo "F claimed a shrink (BAD)"; rc=1;;
    *) echo "F no shrink claim OK";;
esac
assert_eq "$(pt1)" "$BEFORE" "F 拒绝必须发生在写表之前（分区尺寸未变）"

echo
echo "===== G: SIZE 的收缩形式（-100M）：与 --end 共用解析后的末端 ====="
setup_gpt
OUT=$($B resize "$T":1 -100M --yes 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | grep -E 'fs shrunk|refused|error' | sed 's/^/    /'
exp "$E" 0 "相对收缩走同一条离线路径"
case "$OUT" in
    *"fs shrunk to 314572800 bytes"*) echo "G shrink landing OK";;
    *) echo "G shrink landing missing (BAD)"; rc=1;;
esac
assert_eq "$(pt1)" '"size_bytes":314572800' "G 400M - 100M = 300M"

echo
echo "===== H: --dry-run 打印计划、不写盘、不要求 --yes ====="
setup_gpt
BEFORE=$(pt1)
OUT=$($B resize "$T":1 -100M --dry-run 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | sed 's/^/    /'
exp "$E" 0 "预览成功"
case "$OUT" in
    *"plan: shrink partition 1"*) echo "H plan line OK";;
    *) echo "H plan line missing (BAD)"; rc=1;;
esac
case "$OUT" in
    *"dry run: nothing written"*) echo "H stop line OK";;
    *) echo "H stop line missing (BAD)"; rc=1;;
esac
assert_eq "$(pt1)" "$BEFORE" "H 预览不动表"

echo
echo "===== I: MBR 已提交的记录：同一请求可收尾，盘上区间被外部改过则拒绝 ====="
# 现场：FS 与条目都已缩到 300M，记录停在“表已改、收尾未做”
mbr_committed() {
    rm_img "$T"
    truncate -s 512M "$T"
    $B new "$T" --table mbr --yes >/dev/null
    $B add "$T" 400M --align mib --yes >/dev/null 2>&1
    $B mkfs "$T":1 ext4 --yes >/dev/null 2>&1
    DISKEDIT_FAULT=mbr-after-table-write $BF resize "$T":1 300M --yes >/dev/null 2>&1
    exp "$?" 134 "表写之后中断（FS 与条目均已缩到 300M）"
    assert_eq "$(pt1)" '"size_bytes":314572800' "I 条目已缩到 300M"
    assert_eq "$(record_present "$T" && echo yes || echo no)" "yes" "I 记录留在盘上"
}

mbr_committed
OUT=$($B resize "$T":1 300M --yes 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | grep -E 'resize|refused|error' | sed 's/^/    /'
exp "$E" 0 "同一请求的续跑按记录收尾"
assert_eq "$(pt1)" '"size_bytes":314572800' "I 收尾不改条目"
assert_eq "$(record_present "$T" && echo yes || echo no)" "no" "I 收尾后记录已撤"

mbr_committed
# 外部工具把第 1 条目改成 350M（size 字段在 446+12=458，4 字节小端）
printf '\x00\xf0\x0a\x00' | dd of="$T" bs=1 seek=458 conv=notrunc 2>/dev/null
OUT=$($B resize "$T":1 300M --yes 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | sed 's/^/    /'
exp "$E" 3 "盘上区间不是请求目标 ⇒ 拒绝按现状收尾"
case "$OUT" in
    *"layout moved under the job"*) echo "I refusal wording OK";;
    *) echo "I refusal wording missing (BAD)"; rc=1;;
esac
assert_eq "$(pt1)" '"size_bytes":367001600' "拒绝不写表（外部改动保留）"
assert_eq "$(record_present "$T" && echo yes || echo no)" "yes" "拒绝不撤记录"

mbr_committed
# 相对尺寸在提交后按新长度重新锚定：拒绝，而不是按现状收尾
OUT=$($B resize "$T":1 -100M --yes 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | tail -2 | sed 's/^/    /'
exp "$E" 3 "相对尺寸的续跑被拒绝"
assert_eq "$(pt1)" '"size_bytes":314572800' "I 分区停在记录写下的 300M"
assert_eq "$(record_present "$T" && echo yes || echo no)" "yes" "拒绝不撤记录"

mbr_committed
# 外部工具把第 1 条目的起点后移一个扇区（start 字段在 446+8=454，4 字节小端）
printf '\x01\x08\x00\x00' | dd of="$T" bs=1 seek=454 conv=notrunc 2>/dev/null
OUT=$($B resize "$T":1 300M --yes 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | tail -2 | sed 's/^/    /'
exp "$E" 3 "起点被外部移动 ⇒ 拒绝按现状收尾"
case "$OUT" in
    *"layout moved under the job"*) echo "I start refusal wording OK";;
    *) echo "I start refusal wording missing (BAD)"; rc=1;;
esac
assert_eq "$(pt1)" '"size_bytes":314572800' "拒绝不写表（起点改动保留）"
assert_eq "$(record_present "$T" && echo yes || echo no)" "yes" "拒绝不撤记录"

echo
echo "===== 收尾与残留核对 ====="
rm -rf "$FAKE" 2>/dev/null
echo "本镜像 loop: $(losetup -j "$T" -n -O NAME 2>/dev/null | wc -l)  fake bin: $(ls -d "$FAKE" 2>/dev/null | wc -l)"
exit $rc