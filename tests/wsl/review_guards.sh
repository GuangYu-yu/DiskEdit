#!/usr/bin/env bash
# 评审闸口回归：三条"必须拒绝且不写盘"的守卫。
#   E1 GPT 表项重叠 ⇒ 文件系统写命令拒绝（分区字节区间的唯一出口处拦截）
#   E2 `new` 的目标是分区节点 ⇒ 拒绝（建表会覆盖该分区的数据）
#   E5 未收尾的搬移作业挡住反向（缩容）resize ⇒ 拒绝，不得把缩容执行成旧扩容
# 三条都以"退出 10 + 目标字节不变"为判据；E2 需要 losetup（分区节点），
# E1 需要 python3 手工构造一份 CRC 自洽但条目重叠的表，故本脚本须以 root 运行。
source "$(dirname "$0")/lib.sh"
require_fault_bin

if ! command -v python3 >/dev/null 2>&1; then
    echo "SKIP: python3 缺失，无法构造 CRC 自洽的重叠 GPT"
    exit 0
fi

T1=/var/tmp/rev_g1.img
T2=/var/tmp/rev_g2.img
T5=/var/tmp/rev_g5.img
track_file "$T1"; track_file "$T2"; track_file "$T5"
rm -f "$T1" "$T1"$SIDECAR_GLOB "$T2" "$T2"$SIDECAR_GLOB "$T5" "$T5"$SIDECAR_GLOB 2>/dev/null

echo "########## E1: GPT 表项重叠 ⇒ FS 写命令拒绝且不写盘 ##########"
truncate -s 256M "$T1"
$B new "$T1" --yes >/dev/null
$B add "$T1" --start 2048 --end 65535 --name p1 >/dev/null
$B add "$T1" --start 65536 --end 131071 --name p2 >/dev/null
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
E1B=$(md5sum "$T1" | cut -d' ' -f1)
OUT=$($B mkfs "$T1":1 ext4 --yes 2>&1); E=$?
echo "mkfs on overlapping table -> exit=$E : $(echo "$OUT" | tail -1)"
exp "$E" 10 "E1 overlapping table refuses the FS write"
case "$OUT" in *overlap*) echo "  OK   E1 refusal names the overlap" ;; *) echo "  BAD  E1 refusal does not name the overlap"; rc=1 ;; esac
assert_eq "$(md5sum "$T1" | cut -d' ' -f1)" "$E1B" "E1 image bytes unchanged after the refusal"

echo
echo "########## E2: new 的目标是分区节点 ⇒ 拒绝且不写盘 ##########"
truncate -s 256M "$T2"
$B new "$T2" --yes >/dev/null
$B add "$T2" --start 2048 --end 65535 --name p1 >/dev/null
$B add "$T2" --start 65536 --end 131071 --name p2 >/dev/null
LD=$(lo_attach "$T2")
if lo_waitpart "${LD}p2"; then
    mkfs.ext4 -q -F "${LD}p2" >/dev/null 2>&1
    E2B=$(md5sum "${LD}p2" | cut -d' ' -f1)
    OUT=$($B new "${LD}p2" --table gpt --yes 2>&1); E=$?
    echo "new on partition node -> exit=$E : $(echo "$OUT" | tail -1)"
    exp "$E" 10 "E2 partition node refused as a new target"
    case "$OUT" in *"target is a partition, not a whole disk"*) echo "  OK   E2 refusal names the partition node" ;; *) echo "  BAD  E2 refusal wording"; rc=1 ;; esac
    assert_eq "$(md5sum "${LD}p2" | cut -d' ' -f1)" "$E2B" "E2 partition node bytes unchanged after the refusal"
else
    echo "  BAD  ${LD}p2 did not appear"; rc=1
fi
lo_detach "$LD"

echo
echo "########## E5: 未收尾搬移作业挡住反向（缩容）resize ##########"
truncate -s 512M "$T5"
$BF new "$T5" --yes >/dev/null
$BF create "$T5" --size 100M --name p1 --fs ext4 >/dev/null
$BF add "$T5" --start 307200 --end 512000 --name p2 >/dev/null
# 造未收尾的 Shift 作业：精确 SIZE 扩容、挡路分区搬到一半即 abort，现场保留（不 abandon）
DISKEDIT_FAULT=chunk:20 $BF resize "$T5":1 209715200 --allow-move --yes >/dev/null 2>&1
exp "$?" 134 "E5 grow interrupted mid-move"
[ -f "$T5$CKPT_SUFFIX" ] && echo "  OK   relocation checkpoint present after the interruption" || { echo "  BAD  no checkpoint after the interruption"; rc=1; }
OUT=$($BF resize "$T5":1 52428800 2>&1); E=$?
echo "shrink against the pending job -> exit=$E : $(echo "$OUT" | tail -1)"
exp "$E" 10 "E5 shrink refused instead of replaying the grow plan"
case "$OUT" in *"owns this target"*) echo "  OK   E5 refusal names the pending owner" ;; *) echo "  BAD  E5 refusal wording"; rc=1 ;; esac
case "$OUT" in *"diskedit abandon"*) echo "  OK   E5 refusal points at abandon" ;; *) echo "  BAD  E5 refusal lacks the abandon exit"; rc=1 ;; esac

echo
exit $rc