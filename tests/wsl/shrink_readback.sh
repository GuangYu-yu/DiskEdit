#!/usr/bin/env bash
# 缩容后回读**实际落点**：工具成功返回只说明它没报错，不说明 FS 落在我们请求的尺寸上。
# 覆盖四条：正常回读（请求值 = 实际落点）、工具向下取整（实际 < 请求值）、
# 回读到的落点大于请求值（工具行为不符合预期，分区不缩）、回读拿不到落点
# （缩容可能已完成但落点无法证实：表不写、分区保持原尺寸，落一条待办而不是报失败）。
# GPT 路径（movepart::execute_resize）与 MBR 路径（cmd/resize）各覆盖一次。
#
# 假 dumpe2fs 靠工具表变体生效：工具解析在编译期钉死（src/fsops/env.rs 的 TOOL_PATH），
# PATH 影子不参与。BFK 的表把假目录排在系统目录之前，其余工具仍落到真身。
source "$(dirname "$0")/lib.sh"
require_fake_bin

T=/var/tmp/t42.img
track_file "$T"

FAKE=$FAKE_TOOL_DIR
cleanup_hook() { rm -rf "$FAKE" 2>/dev/null; }

# 假工具：只覆盖回读要用的那一个可执行文件，其余仍走工具表里的系统目录
fake() { # $1=名  $2=脚本体
    printf '%s\n' "$2" > "$FAKE/$1"
    chmod +x "$FAKE/$1"
}

setup_gpt() {
    rm -f "$T" "$T"$SIDECAR_GLOB
    truncate -s 512M "$T"
    $B new "$T" --yes >/dev/null
    $B create "$T" --size 400M --name p1 --fs ext4 >/dev/null 2>&1
}

# 分区 1 的字节数（info 输出里盘与分区各有一处 size_bytes，取分区那条）
pt1() { $B info "$T" | grep -o '"num":1,[^}]*}' | grep -o '"size_bytes":[0-9]*'; }

echo "===== A: ext 缩容，回读正常 → 成功行同时给请求值与实际落点 → 0 ====="
setup_gpt
OUT=$($B resize "$T":1 300M 2>&1); E=$?
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
OUT=$($B resize "$T":1 "$REQ" 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | grep -E 'fs shrunk|refused|error' | sed 's/^/    /'
exp "$E" 0 "取整落点仍在请求范围内即成功"
case "$OUT" in
    *"fs shrunk to 104857600 bytes (requested 104858112)"*) echo "B rounding visible OK";;
    *) echo "B rounding not reported as expected (BAD)"; rc=1;;
esac

echo
echo "===== C: 回读落点大于请求值 → 工具行为不符合预期，分区不缩 → 30 ====="
setup_gpt
BEFORE=$(pt1)
rm -rf "$FAKE"; mkdir -p "$FAKE"
# Block size 保持真值：缩容前的 min-size 预查（resize2fs -P × dumpe2fs 块大小）要用它；
# 回读取的 Block count 故意报一个大数
fake dumpe2fs '#!/bin/sh
echo "Block count:              999999999"
echo "Block size:               4096"'
OUT=$($BFK resize "$T":1 300M 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | sed 's/^/    /'
exp "$E" 30 "落点超出请求值不是成功（可能已写盘，带警示）"
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
echo "===== D: 回读拿不到落点 → 落一条 fs shrink 待办，表不写、分区保持原尺寸 → 20 ====="
setup_gpt
rm -rf "$FAKE"; mkdir -p "$FAKE"
# 只给 Block size（预查要用），回读要的 Block count 缺失 = 回读拿不到落点
fake dumpe2fs '#!/bin/sh
echo "Block size:               4096"'
OUT=$($BFK resize "$T":1 300M 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | sed 's/^/    /'
exp "$E" 20 "回读不出来不是缩容失败，改出一条待办"
case "$OUT" in *"fs shrink"*) echo "D pending labelled OK";; *) echo "D pending missing (BAD)"; rc=1;; esac
case "$OUT" in *"could not be read back"*) echo "D unverifiable wording OK";; *) echo "D wording missing (BAD)"; rc=1;; esac
case "$OUT" in
    *"run manually: dumpe2fs -h <partition device>"*) echo "D remedy OK";;
    *) echo "D remedy missing (BAD)"; rc=1;;
esac
assert_eq "$(pt1)" '"size_bytes":419430400' "D 落点未证实不写表（分区保持原尺寸）"

echo
echo "===== E: MBR 路径同样报出请求值与实际落点 → 0 ====="
rm -f "$T" "$T"$SIDECAR_GLOB
truncate -s 512M "$T"
$B new "$T" --table msdos --yes >/dev/null
$B create "$T" --size 400M --fs ext4 >/dev/null 2>&1
OUT=$($B resize "$T":1 300M 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | sed 's/^/    /'
exp "$E" 0 "MBR 缩容成功"
case "$OUT" in
    *"fs shrunk to 314572800 bytes (requested 314572800)"*) echo "E both sizes OK";;
    *) echo "E size line missing (BAD)"; rc=1;;
esac

echo
echo "===== 收尾与残留核对 ====="
rm -rf "$FAKE" 2>/dev/null
echo "本镜像 loop: $(losetup -j "$T" -n -O NAME 2>/dev/null | wc -l)  fake bin: $(ls -d "$FAKE" 2>/dev/null | wc -l)"
exit $rc