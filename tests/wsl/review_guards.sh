#!/usr/bin/env bash
# 评审闸口回归：三条"必须拒绝且不写盘"的守卫。
#   E1 GPT 表项重叠 ⇒ 文件系统写命令拒绝（分区字节区间的唯一出口处拦截）
#   E2 `new` 的目标是分区节点 ⇒ 拒绝（建表会覆盖该分区的数据）
#   E5 未收尾的搬移作业挡住反向（缩容）resize ⇒ 拒绝，不得把缩容执行成旧扩容
# 三条都以"退出 3 + 目标字节不变"为判据；E2 需要 losetup（分区节点），
# E1 需要 python3 手工构造一份 CRC 自洽但条目重叠的表，故本脚本须以 root 运行。
source "$(dirname "$0")/lib.sh"
require_fault_bin

if ! command -v python3 >/dev/null 2>&1; then
    echo "SKIP: python3 缺失，无法构造 CRC 自洽的重叠 GPT"
    exit 0
fi

img T1 rev_g1.img
img T2 rev_g2.img
img T5 rev_g5.img
track_file "$T1"; track_file "$T2"; rm -rf "$T1" "$T1"$SIDECAR_GLOB "$T2" "$T2"$SIDECAR_GLOB "$T5" "$T5"$SIDECAR_GLOB 2>/dev/null

echo "########## E1: GPT 表项重叠 ⇒ FS 写命令拒绝且不写盘 ##########"
truncate -s 256M "$T1"
$B new "$T1" --yes >/dev/null
$B add --yes "$T1" 32505856 --start 2048 --name p1 >/dev/null
$B add --yes "$T1" 33554432 --start 65536 --name p2 >/dev/null
# 把条目 2 的 first_lba 改成 32768 ⇒ 与条目 1（2048..65535）重叠。主备两份条目数组
# 与各自表头的 CRC 必须一并重算：只改一处会让该副本被判损坏，工具从另一副本恢复，
# 重叠被掩盖而命令照常执行——那样测的就不是本条闸口
python3 - "$T1" <<'PY'
import struct, sys, zlib

f = open(sys.argv[1], 'r+b')
ss = 512

def rd(off, n):
    f.seek(off); return f.read(n)

def wr(off, b):
    f.seek(off); f.write(b)

hdr = bytearray(rd(ss, ss))
hsize = struct.unpack_from('<I', hdr, 12)[0]
ent_lba = struct.unpack_from('<Q', hdr, 72)[0]
num = struct.unpack_from('<I', hdr, 80)[0]
esz = struct.unpack_from('<I', hdr, 84)[0]
backup_lba = struct.unpack_from('<Q', hdr, 32)[0]
alen = num * esz

arr = bytearray(rd(ent_lba * ss, alen))
struct.pack_into('<Q', arr, esz * 1 + 32, 32768)
wr(ent_lba * ss, arr)
struct.pack_into('<I', hdr, 88, zlib.crc32(bytes(arr)) & 0xffffffff)
struct.pack_into('<I', hdr, 16, 0)
struct.pack_into('<I', hdr, 16, zlib.crc32(bytes(hdr[:hsize])) & 0xffffffff)
wr(ss, hdr)

bh = bytearray(rd(backup_lba * ss, ss))
bent = struct.unpack_from('<Q', bh, 72)[0]
barr = bytearray(rd(bent * ss, alen))
struct.pack_into('<Q', barr, esz * 1 + 32, 32768)
wr(bent * ss, barr)
struct.pack_into('<I', bh, 88, zlib.crc32(bytes(barr)) & 0xffffffff)
struct.pack_into('<I', bh, 16, 0)
struct.pack_into('<I', bh, 16, zlib.crc32(bytes(bh[:hsize])) & 0xffffffff)
wr(backup_lba * ss, bh)
f.flush(); f.close()
PY
E1B=$(mdsum "$T1")
OUT=$($B mkfs "$T1":1 ext4 --yes 2>&1); E=$?
echo "mkfs on overlapping table -> exit=$E : $(echo "$OUT" | tail -1)"
exp "$E" 3 "E1 overlapping table refuses the FS write"
case "$OUT" in *overlap*) echo "  OK   E1 refusal names the overlap" ;; *) echo "  BAD  E1 refusal does not name the overlap"; rc=1 ;; esac
assert_eq "$(mdsum "$T1")" "$E1B" "E1 image bytes unchanged after the refusal"

echo
echo "########## E2: new 的目标是分区节点 ⇒ 拒绝且不写盘 ##########"
truncate -s 256M "$T2"
$B new "$T2" --yes >/dev/null
$B add --yes "$T2" 32505856 --start 2048 --name p1 >/dev/null
$B add --yes "$T2" 33554432 --start 65536 --name p2 >/dev/null
LD=$(lo_attach "$T2")
if lo_waitpart "${LD}p2"; then
    mkfs.ext4 -q -F "${LD}p2" >/dev/null 2>&1
    E2B=$(mdsum "${LD}p2")
    OUT=$($B new "${LD}p2" --table gpt --yes 2>&1); E=$?
    echo "new on partition node -> exit=$E : $(echo "$OUT" | tail -1)"
    exp "$E" 3 "E2 partition node refused as a new target"
    case "$OUT" in *"target is a partition, not a whole disk"*) echo "  OK   E2 refusal names the partition node" ;; *) echo "  BAD  E2 refusal wording"; rc=1 ;; esac
    assert_eq "$(mdsum "${LD}p2")" "$E2B" "E2 partition node bytes unchanged after the refusal"

    # 同一道分区节点守卫还覆盖 add / mkfs / undo / abandon（建表 / 回放类命令的载体必须是整盘）。
    # 分区节点在位才可测：节点未就绪时这些调用对着不存在的设备，退出码不构成任何断言
    OUT=$($B add --yes "${LD}p2" 1048576 --start 2048 --name p1 2>&1); E=$?
    echo "add on partition node -> exit=$E"; exp "$E" 3 "E2a add refused on a partition node"
    case "$OUT" in *"target is a partition, not a whole disk"*) echo "  OK   E2a refusal names the partition node" ;; *) echo "  BAD  E2a refusal wording"; rc=1 ;; esac

    OUT=$($B mkfs "${LD}p2":1 ext4 --yes 2>&1); E=$?
    echo "mkfs on partition node -> exit=$E"; exp "$E" 3 "E2b mkfs refused on a partition node"
    case "$OUT" in *"target is a partition, not a whole disk"*) echo "  OK   E2b refusal names the partition node" ;; *) echo "  BAD  E2b refusal wording"; rc=1 ;; esac

    OUT=$($B undo "${LD}p2" --yes 2>&1); E=$?
    echo "undo on partition node -> exit=$E"; exp "$E" 3 "E2c undo refused on a partition node"
    case "$OUT" in *"target is a partition, not a whole disk"*) echo "  OK   E2c refusal names the partition node" ;; *) echo "  BAD  E2c refusal wording"; rc=1 ;; esac

    OUT=$($B abandon "${LD}p2" 2>&1); E=$?
    echo "abandon on partition node -> exit=$E"; exp "$E" 3 "E2d abandon refused on a partition node"
    case "$OUT" in *"target is a partition, not a whole disk"*) echo "  OK   E2d refusal names the partition node" ;; *) echo "  BAD  E2d refusal wording"; rc=1 ;; esac

    assert_eq "$(mdsum "${LD}p2")" "$E2B" "E2 all refusals leave the partition node bytes unchanged"
else
    echo "  BAD  ${LD}p2 did not appear"; rc=1
fi
lo_detach "$LD"

echo
echo "########## E5: 未收尾搬移作业挡住反向（缩容）resize ##########"
truncate -s 512M "$T5"
$BF new "$T5" --yes >/dev/null
$BF add "$T5" 100M --align mib --name p1 --yes >/dev/null
$BF mkfs "$T5":1 ext4 --yes >/dev/null
$BF add --yes "$T5" 104858112 --start 307200 --name p2 >/dev/null
# 造未收尾的 Shift 作业：精确 SIZE 扩容、挡路分区搬到一半即 abort，现场保留（不 abandon）。
# p2 的搬移量为 104448 扇区（≈50MiB），4MiB chunk 共 13 块且自重叠逐块落进度——注入点取块数之内
DISKEDIT_FAULT=chunk:8 $BF resize "$T5":1 209715200 --allow-move --yes >/dev/null 2>&1
exp "$?" 134 "E5 grow interrupted mid-move"
record_present "$T5" && echo "  OK   relocation record present after the interruption" || { echo "  BAD  no record after the interruption"; rc=1; }
# 缩容请求走 resize-part（--end）：resize 本身只扩不缩，那条指引轮不到 busy 闸；
# resize-part 的闸在记录准入，未收尾的搬移作业按 busy 拒
OUT=$($BF resize-part "$T5":1 --end 104447 --yes 2>&1); E=$?
echo "shrink against the pending job -> exit=$E : $(echo "$OUT" | tail -1)"
# 未收尾作业的记录已写盘（provisional）：挡路拒绝按 past-boundary 口径报 5
exp "$E" 5 "E5 shrink refused instead of replaying the grow plan"
case "$OUT" in *"owns this target"*) echo "  OK   E5 refusal names the pending owner" ;; *) echo "  BAD  E5 refusal wording"; rc=1 ;; esac
case "$OUT" in *"diskedit abandon"*) echo "  OK   E5 refusal points at abandon" ;; *) echo "  BAD  E5 refusal lacks the abandon exit"; rc=1 ;; esac

echo
echo "########## E3: 块设备搬移中断续跑（条目已提交、表步未做）##########"
# 中断点取 after-entry-commit:0：搬移条目已提交、表步尚未做。
# 契约：resume 完成表步后分区已在 400000——退 3 的语义是"未写盘且请求与现状不匹配"，
# 而续跑时盘上真相与请求一致——3 是码与理由双错配
img T3 rev_g3.img
rm -rf "$T3" "$T3"$SIDECAR_GLOB 2>/dev/null
truncate -s 512M "$T3"
$BF new "$T3" --yes >/dev/null
$BF add --yes "$T3" 49283072 --start 2048 --name p1 >/dev/null
$BF add --yes "$T3" 50331648 --start 200704 --name p2 >/dev/null
LD=$(lo_attach "$T3")
D=$(basename "$LD")
if lo_waitpart "${LD}p1"; then
    echo "  sysfs p1 start before = $(cat "/sys/block/$D/${D}p1/start" 2>/dev/null)"
    # move 走执行器：条目提交后、表步之前的注入点（rs-* 系列只在 resize-part 路径）。
    # 中断后表未提交 → resume 完成表步、分区到 400000 → 输出非 3 即契约
    DISKEDIT_FAULT=after-entry-commit:0 $BF move "$LD":1 --start 400000 --yes >/dev/null 2>&1
    exp "$?" 134 "E3 move interrupted after the entry commit (table step pending)"
    echo "  on-disk p1 = $($B info "$T3" | grep -o '"num":1,[^}]*}')"
    echo "  sysfs p1 start after the abort = $(cat "/sys/block/$D/${D}p1/start" 2>/dev/null)"
    OUT=$($BF move "$LD":1 --start 400000 --yes 2>&1); E=$?
    echo "  resume -> exit=$E : $(echo "$OUT" | tail -1)"
    if [ "$E" = 3 ]; then
        echo "  BAD  E3 resume refused with 3 — a stale kernel view is not a request mismatch"
        rc=1
    else
        echo "  OK   E3 resume not misreported as a refusal (exit=$E)"
    fi
else
    echo "  BAD  ${LD}p1 did not appear"; rc=1
fi
lo_detach "$LD"

echo
echo "########## E6: abandon 后重跑多挡路者 Shift 作业（不得二次位移）##########"
# 判据：abandon 后原样重跑，最终布局必须与"一次做成"逐条相同。挡路者被按同一
# delta 二次位移时，表项会比正确结果更靠右——这是可判定的差异，不必依赖数据哈希
img T6 rev_g6.img
img T6X rev_g6x.img
track_file "$T6"; mk6() { # $1=img
    rm -rf "$1" "$1"$SIDECAR_GLOB 2>/dev/null
    truncate -s 4G "$1"
    $BF new "$1" --yes >/dev/null
    $BF add "$1" 300M --align mib --name p1 --yes >/dev/null
    $BF add "$1" 300M --align mib --name p2 --yes >/dev/null
    $BF add "$1" 300M --align mib --name p3 --yes >/dev/null
}
sig() { # $1=img → 各条目起止的紧凑签名
    $B info "$1" 2>/dev/null | grep -o '"num":[0-9]*,"first_lba":[0-9]*,"last_lba":[0-9]*'
}
mk6 "$T6X"
$BF resize "$T6X":1 1000M --allow-move --yes --no-fs >/dev/null 2>&1
exp "$?" 0 "E6 reference run (single pass) completes"
SIG_REF=$(sig "$T6X")
echo "  reference layout:"; echo "$SIG_REF" | sed 's/^/    /'
mk6 "$T6"
DISKEDIT_FAULT=after-entry-commit:0 $BF resize "$T6":1 1000M --allow-move --yes --no-fs >/dev/null 2>&1
exp "$?" 134 "E6 shift interrupted after the first entry commit"
$BF abandon "$T6" >/dev/null 2>&1
exp "$?" 0 "E6 abandon releases the scene"
OUT=$($BF resize "$T6":1 1000M --allow-move --yes --no-fs 2>&1); E=$?
echo "  rerun after abandon -> exit=$E : $(echo "$OUT" | tail -1)"
if [ "$E" = 0 ]; then
    SIG_GOT=$(sig "$T6")
    echo "  rerun layout:"; echo "$SIG_GOT" | sed 's/^/    /'
    assert_eq "$SIG_GOT" "$SIG_REF" "E6 rerun reproduces the single-pass layout"
else
    echo "  OK   E6 rerun refused the ambiguous state instead of relocating blockers twice"
fi

echo
echo "########## E4: 在线写表期间第三方创建分区（邻居集陈旧）##########"
# 并发注入：在线路径的邻居集取自 sysfs 快照，sfdisk 前的重验只覆盖目标分区。
# 判定只在"真的看到重叠表"时失败——没撞上窗口不算证据，但撞上了就是硬缺陷
img T4 rev_g4.img
overlap_hits=0
for i in 1 2 3 4 5 6 7 8; do
    rm -rf "$T4" "$T4"$SIDECAR_GLOB 2>/dev/null
    truncate -s 2G "$T4"
    $B new "$T4" --yes >/dev/null
    $B add "$T4" 500M --align mib --name p1 --yes >/dev/null
    $B mkfs "$T4":1 ext4 --yes >/dev/null
    LD=$(lo_attach "$T4")
    if ! lo_waitpart "${LD}p1" || ! mount_at "${LD}p1" "$TEST_MNT"; then
        echo "  (attempt $i) loop/mount unavailable"
        lo_detach "$LD"; continue
    fi
    $BF resizefs --yes "$TEST_MNT" 1610612736 >/dev/null 2>&1 &
    rp=$!
    # 第三方（不守本工具的锁）在 p1 将要吞掉的区间里建条目
    for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do
        printf 'start=1640448, size=204800, type=linux\n' | sfdisk --no-reread --force -N 2 "$LD" >/dev/null 2>&1
    done
    wait "$rp" 2>/dev/null
    unmount_now "$TEST_MNT"
    lo_detach "$LD"
    if $B info "$T4" 2>/dev/null | grep -q overlap; then
        overlap_hits=$((overlap_hits + 1))
        echo "  (attempt $i) OVERLAP: $($B info "$T4" 2>/dev/null | grep -o 'partition entries #[0-9]* and #[0-9]* overlap' | head -1)"
    fi
done
if [ "$overlap_hits" -gt 0 ]; then
    echo "  BAD  E4 produced an overlapping table in $overlap_hits of 8 attempts"
    rc=1
else
    echo "  OK   E4 no overlap observed in 8 attempts (window not hit — not proof of absence)"
fi

echo
exit $rc