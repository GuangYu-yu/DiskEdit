#!/usr/bin/env bash
# 临时挂载点清理失败的运行时验证：卸载的成败与核心操作分属两个事实——
# 清理待办随 Ok 浮出（已有 pending 通道的路径进 20），核心失败时主错误分类不变；
# umount 非零退出与 umount 缺失都产生待办。
#
# 与 postcondition_contract.sh 的分工：那边覆盖 FS 步失败（PendingKind::Fs），
# 本脚本专测卸载这一步本身失败（PendingKind::Other("mount cleanup")）。
# btrfs 扩容走 with_mount：真实 mount → btrfs filesystem resize → umount，是本项的抓手。
source "$(dirname "$0")/lib.sh"
require_bin

T=/var/tmp/t41.img
track_file "$T"

# diskedit 的临时挂载点（mount_point()：$TMPDIR/diskedit.mnt.<pid>.<seq>）不在
# _TRACK_MOUNTS 里：假 umount 场景下它真的留在挂载态，必须在这里用真 umount 收掉
FAKE=/var/tmp/diskedit-fakebin
cleanup_hook() {
    local mp
    for mp in /tmp/diskedit.mnt.*; do
        [ -e "$mp" ] || continue
        /usr/bin/umount "$mp" 2>/dev/null || /bin/umount "$mp" 2>/dev/null
        rmdir "$mp" 2>/dev/null
    done
    rm -rf "$FAKE" 2>/dev/null
}

setup_case() {
    rm -f "$T" "$T"$SIDECAR_GLOB
    truncate -s 2G "$T"
    $B new "$T" --yes >/dev/null
    $B create "$T" --size 400M --name b --fs btrfs >/dev/null 2>&1
}

fake() { # $1=名  $2=脚本体
    printf '%s\n' "$2" > "$FAKE/$1"
    chmod +x "$FAKE/$1"
}

# 分区 1 的字节数（info 输出里盘与分区各有一处 size_bytes，取分区那条）
pt() { $B info "$T" | grep -o '"num":1,[^}]*}' | grep -o '"size_bytes":[0-9]*'; }

echo "===== A: btrfs 扩容，umount 正常 → 后置条件全满足 → 0 ====="
setup_case
OUT=$($B resize "$T":1 1500M 2>&1); E=$?
echo "exit=$E"
exp "$E" 0 "清理也成功时仍是完整成功"
case "$OUT" in
    *"mount cleanup"*) echo "A unexpected cleanup report (BAD)"; rc=1;;
    *) echo "A no cleanup report OK";;
esac

echo
echo "===== B: btrfs 扩容 + umount 非零退出 → PARTIAL(20) + mount cleanup ====="
setup_case
rm -rf "$FAKE"; mkdir -p "$FAKE"
fake umount '#!/bin/sh
echo "target is busy" >&2
exit 32'
OUT=$(PATH="$FAKE:$PATH" $B resize "$T":1 1500M 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | grep -E 'mount cleanup|follow-up|run manually|left behind' | sed 's/^/    /'
exp "$E" 20 "卸载失败不改写扩容成功，只落一条待办"
case "$OUT" in *"mount cleanup"*) echo "B cleanup reported OK";; *) echo "B cleanup report missing (BAD)"; rc=1;; esac
case "$OUT" in *"target is busy"*) echo "B tool words OK";; *) echo "B tool words missing (BAD)"; rc=1;; esac
case "$OUT" in *"run manually: umount /tmp/diskedit.mnt."*) echo "B remedy OK";; *) echo "B remedy missing (BAD)"; rc=1;; esac

echo
echo "===== C: umount 缺失 → 事前拒绝(30)，未写盘也就没有待办 ====="
# umount 缺失是**事前可知**的：mount 层工具前置检查（check_mount_layer_tools）在写盘前
# 就拒绝，比"事后留一条清理待办"更严——不留残留挂载点、也不动盘
setup_case
BEFORE=$(pt)
# PATH 只留本目录：umount 不在其中，其余所需工具以符号链接补上
rm -rf "$FAKE"; mkdir -p "$FAKE"
for t in losetup mount btrfs udevadm blockdev; do
    p=$(command -v "$t") && ln -sf "$p" "$FAKE/$t"
done
OUT=$(PATH="$FAKE" $B resize "$T":1 1500M 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | sed 's/^/    /'
exp "$E" 30 "umount 缺失在写盘前被拒（30）"
if echo "$OUT" | grep -q 'requires `umount`'; then
    echo "C preflight names the tool OK"
else
    echo "C preflight message missing (BAD)"; rc=1
fi
case "$OUT" in *"mount cleanup"*) echo "C must not report a pending (BAD)"; rc=1;; *) echo "C no pending OK";; esac
assert_eq "$(pt)" "$BEFORE" "C 事前拒绝不得改盘"

echo
echo "===== D: btrfs 扩容失败 + umount 失败 → 主错误仍是 fs grow，分类不变 ====="
setup_case
rm -rf "$FAKE"; mkdir -p "$FAKE"
fake btrfs '#!/bin/sh
echo "simulated btrfs failure" >&2
exit 1'
fake umount '#!/bin/sh
echo "target is busy" >&2
exit 32'
OUT=$(PATH="$FAKE:$PATH" $B resize "$T":1 1500M 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | grep -E 'fs grow|mount cleanup|left behind|follow-up' | sed 's/^/    /'
exp "$E" 20 "核心失败仍按 FS 待办报，清理告警不改写它"
case "$OUT" in *"fs grow"*) echo "D core error kept OK";; *) echo "D core error lost (BAD)"; rc=1;; esac
case "$OUT" in *"left behind"*) echo "D cleanup warning OK";; *) echo "D cleanup warning missing (BAD)"; rc=1;; esac
case "$OUT" in *"mount cleanup"*) echo "D cleanup must not become a pending (BAD)"; rc=1;; *) echo "D cleanup kept out of pending OK";; esac

echo
echo "===== E: resizefs 扩容 + umount 失败 → 成功出口保持 0，只出 warning ====="
setup_case
# 先只改表把分区放大、FS 仍停在 400M，给 resizefs 留下可扩的那段
$B resize "$T":1 1500M --no-fs >/dev/null 2>&1
rm -rf "$FAKE"; mkdir -p "$FAKE"
fake umount '#!/bin/sh
echo "target is busy" >&2
exit 32'
OUT=$(PATH="$FAKE:$PATH" $B resizefs "$T":1 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | sed 's/^/    /'
exp "$E" 0 "resizefs 的成功出口不受清理失败影响"
case "$OUT" in *"left behind"*) echo "E cleanup warning OK";; *) echo "E cleanup warning missing (BAD)"; rc=1;; esac
case "$OUT" in *"mount cleanup"*) echo "E must not become a pending (BAD)"; rc=1;; *) echo "E no pending OK";; esac

echo
echo "===== F: superfloppy(btrfs 整盘) 扩容 + umount 失败 → 成功出口保持 0，只出 warning ====="
rm -f "$T" "$T"$SIDECAR_GLOB
truncate -s 400M "$T"
mkfs.btrfs -q -f "$T" >/dev/null 2>&1
truncate -s 800M "$T"
rm -rf "$FAKE"; mkdir -p "$FAKE"
fake umount '#!/bin/sh
echo "target is busy" >&2
exit 32'
OUT=$(PATH="$FAKE:$PATH" $B resize "$T" 800M 2>&1); E=$?
echo "exit=$E"
echo "$OUT" | sed 's/^/    /'
exp "$E" 0 "superfloppy 的成功出口不受清理失败影响"
case "$OUT" in *"left behind"*) echo "F cleanup warning OK";; *) echo "F cleanup warning missing (BAD)"; rc=1;; esac

echo
echo "===== 收尾与残留核对 ====="
# 假 umount 场景确实把临时挂载点留在了挂载态（这正是被测行为），先按 cleanup_hook
# 的同一动作收干净，再核对"收尾之后"的状态——顺序反了会把残留当常态
for mp in /tmp/diskedit.mnt.*; do
    [ -e "$mp" ] || continue
    /usr/bin/umount "$mp" 2>/dev/null || /bin/umount "$mp" 2>/dev/null
    rmdir "$mp" 2>/dev/null
done
for d in $(losetup -j "$T" -n -O NAME 2>/dev/null); do losetup -d "$d" 2>/dev/null; done
rm -rf "$FAKE" 2>/dev/null
echo "本镜像 loop: $(losetup -j "$T" -n -O NAME 2>/dev/null | wc -l)  temp mounts: $(ls -d /tmp/diskedit.mnt.* 2>/dev/null | wc -l)  fake bin: $(ls -d "$FAKE" 2>/dev/null | wc -l)"
exit $rc