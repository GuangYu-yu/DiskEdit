#!/usr/bin/env bash
# 中断窗口：after-repair（初始进度前）/ after-swap-commit（进度推进前），
# 以及记录不可读时的硬拒（G）
source "$(dirname "$0")/lib.sh"
require_fault_bin

echo "===== E: after-repair（修复完成、初始进度写入前） ====="
img T t17e.img
rm -rf "$T" "$T"$SIDECAR_GLOB
truncate -s 1G "$T"
$BF new "$T" --yes >/dev/null
$BF add "$T" 300M --align mib --name p1 --yes >/dev/null
$BF add "$T" 300M --align mib --name p2 --yes >/dev/null
$BF mkfs "$T":2 ext4 --yes >/dev/null
truncate -s 1200M "$T"   # 预扩容器 → backup GPT/PMBR 过期 → 触发 repair 路径
LD=$(lo_attach "$T")
mount_at "${LD}p2" "$TEST_MNT" && dd if=/dev/urandom of="$TEST_MNT/b2.bin" bs=1M count=250 2>/dev/null; umount "$TEST_MNT"; MD2=$(mdsum "${LD}p2"); lo_detach "$LD"
# p1 是 grow 目标且没有 FS（E 段只有 p2 是 ext4）：扩分区表要显式 --no-fs
ABORT_OUT=$(DISKEDIT_FAULT=after-repair $BF resize "$T":1 +100M --allow-move --yes --no-fs 2>&1)
ABORT_E=$?
# 注入未命中就是这条用例没测到东西：不能只打一行提醒，否则后面的"续跑收敛"恒真
if exp "$ABORT_E" 134 "after-repair 注入命中并中止"; then
    :
else
    printf '%s\n' "$ABORT_OUT" | sed 's/^/    /'
fi
record_present "$T" && echo "  OK   中断留下未收尾记录" || { echo "  BAD  中断后没有记录（无现场可续）"; rc=1; }
# 记录先于 repair 写盘落盘（movepart 绑定顺序）：中断留下未收尾作业，同请求 rerun
# 续跑收敛——"表已提交后 rerun 被拒"属于 resize-part 的 rs-after-commit 语义，
# 这里的中断点在表步之前
OUT=$($BF resize "$T":1 +100M --allow-move --yes --no-fs 2>&1); E=$?
echo "rerun exit=$E : $(echo "$OUT" | head -1)"
exp "$E" 0 "续跑收敛（中断留下的现场被同请求重跑接住）"
echo "$OUT" | grep -q "resuming at entry" && echo "  OK   续跑走记录（resuming at entry）" || { echo "  BAD  续跑未走记录（输出里没有 resuming at entry）"; rc=1; }
LD=$(lo_attach "$T")
[ "$MD2" = "$(mdsum "${LD}p2")" ] && echo "p2 DATA OK" || { echo "p2 DATA CORRUPT"; rc=1; }
e2fsck -fn "${LD}p2" >/dev/null 2>&1 && echo "p2 e2fsck clean" || { echo "p2 e2fsck ISSUES"; rc=1; }
lo_detach "$LD"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "===== F: after-swap-commit（GPT commit 后、记录推进前，重放幂等） ====="
img T t17f.img
rm -rf "$T" "$T"$SIDECAR_GLOB
track_file "$T"
truncate -s 1G "$T"
$BF new "$T" --yes >/dev/null
$BF add "$T" 300M --align mib --name p1 --yes >/dev/null
$BF add "$T" 200M --align mib --name p2 --type-guid 0657fd6d-a4ab-43c4-84e5-0933c84b4f4f --yes >/dev/null
$BF mkfs "$T":2 swap --yes >/dev/null
$BF add "$T" 300M --align mib --name p3 --yes >/dev/null
$BF mkfs "$T":3 ext4 --yes >/dev/null
LD=$(lo_attach "$T")
UUID_BEFORE=$(blkid -s UUID -o value "${LD}p2")
mount_at "${LD}p3" "$TEST_MNT" && dd if=/dev/urandom of="$TEST_MNT/b3.bin" bs=1M count=250 2>/dev/null; umount "$TEST_MNT"; MD3=$(mdsum "${LD}p3"); lo_detach "$LD"
# p1 是 grow 目标且没有 FS：同样要 --no-fs（p2 的 swap 重建属搬移的一环，与本开关无关）
AOUT=$(DISKEDIT_FAULT=after-swap-commit:1 $BF resize "$T":1 +100M --allow-move --yes --no-fs 2>&1); AE=$?
if exp "$AE" 134 "after-swap-commit 注入命中并中止"; then
    :
else
    printf '%s\n' "$AOUT" | sed 's/^/    /'
fi
OUT=$($BF resize "$T":1 +100M --allow-move --yes --no-fs 2>&1); E=$?
echo "$OUT" | grep -o "resuming at entry [0-9]* chunk [0-9]*" | head -1
# 重建分支的身份结论全在 note 里，全文留档：UUID 保持与否的证据链在这份输出里
echo "$OUT" | grep -i "swap\|recreated\|signature" | sed 's/^/    /'
echo "rerun exit=$E"
exp "$E" 0 "重放幂等（同请求重跑完成）"
LD=$(lo_attach "$T")
UUID_AFTER=$(blkid -s UUID -o value "${LD}p2")
[ "$MD3" = "$(mdsum "${LD}p3")" ] && echo "p3 DATA OK" || { echo "p3 DATA CORRUPT"; rc=1; }
e2fsck -fn "${LD}p3" >/dev/null 2>&1 && echo "p3 e2fsck clean" || { echo "p3 e2fsck ISSUES"; rc=1; }
lo_detach "$LD"
# 幂等重建的主链：新落点必须有可激活的 swap 签名
[ -n "$UUID_AFTER" ] && echo "SWAP SIGNATURE REBUILT ($UUID_AFTER)" || { echo "SWAP SIGNATURE MISSING"; rc=1; }
# 换号只允许出现在一种情形：旧位签名已经读不出 UUID（中断点在表提交之后，旧位可能
# 已被同一次搬移覆盖）。这条理由必须由输出自己给出，静默换号仍然是失败
if [ "$UUID_BEFORE" = "$UUID_AFTER" ]; then
    echo "  OK   swap uuid 保持（$UUID_AFTER）"
elif echo "$OUT" | grep -q "old signature had no UUID"; then
    echo "  OK   swap uuid 重新生成（旧位签名已无 UUID，输出已说明）"
else
    echo "  BAD  swap uuid 变化但输出没有说明原因（$UUID_BEFORE -> $UUID_AFTER）"
    rc=1
fi
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "===== G: 记录不可读（头部损坏）→ 重跑硬拒 3 → abandon 释放 → 重跑 ====="
# 搬移目的地与源区不相交（右移越过源末尾）：中断后源数据完好，abandon 后全新重跑
# 才能得到正确结果。若用自重叠右移，中断已覆盖源尾部，abandon 后重跑只会读到坏源
img T t17g.img
rm -rf "$T" "$T"$SIDECAR_GLOB
track_file "$T"
truncate -s 2G "$T"
$BF new "$T" --yes >/dev/null
$BF add "$T" 900M --align mib --name big --yes >/dev/null
$BF mkfs "$T":1 ext4 --yes >/dev/null
LD=$(lo_attach "$T")
lo_waitpart "${LD}p1" || { echo "p1 part node not ready"; rc=1; }
mount_at "${LD}p1" "$TEST_MNT" && dd if=/dev/urandom of="$TEST_MNT/blob.bin" bs=1M count=800 2>/dev/null
umount "$TEST_MNT"
MD=$(mdsum "${LD}p1")
lo_detach "$LD"
# 2000896..3844095 = 2000896 起 1843200 扇区（900MiB，1MiB 对齐），与源区 2048..1845247 不相交
MOVEOUT=$(DISKEDIT_FAULT=chunk:200 $BF move "$T":1 --start 2000896 --chunk 1 --yes 2>&1); MOVE_E=$?
if exp "$MOVE_E" 134 "chunk 注入命中并中止"; then
    :
else
    printf '%s\n' "$MOVEOUT" | sed 's/^/    /'
fi
REC=$(record_file "$T")
echo "record: $REC"
[ -f "$REC" ] && echo "  OK   中断留下未收尾记录" || { echo "  BAD  中断后没有记录（无现场可破坏）"; rc=1; }
# 打坏头 CRC（保留 MAGIC）：记录成为"读不出来"而非"布局不识别"——前者 abandon 可释放，
# 后者所有权验证不过关、abandon 一律拒绝，须手工处理
dd if=/dev/zero of="$REC" bs=1 seek=12 count=4 conv=notrunc 2>/dev/null
OUT=$($BF move "$T":1 --start 2000896 --chunk 1 --yes 2>&1); E=$?
echo "rerun exit=$E : $(echo "$OUT" | head -1)"
exp "$E" 3 "不可读记录时重跑被拒（不得静默重跑）"
echo "$OUT" | grep -q "recovery record, unreadable" && echo "$OUT" | grep -q "diskedit abandon" && echo "G UNREADABLE-RECORD REPORT OK" || { echo "G report missing (BAD)"; rc=1; }
exp "$($B abandon "$T" --discard-replay >/dev/null 2>&1; echo $?)" 0 "abandon 释放不可读现场"
OUT=$($BF move "$T":1 --start 104448 --chunk 1 --yes 2>&1); exp "$?" 0 "abandon 后重跑完成"
record_absent "$T" && echo "  OK   完成后无记录残留" || { echo "  BAD  完成后记录仍在"; rc=1; }
LD=$(lo_attach "$T")
lo_waitpart "${LD}p1" || { echo "p1 part node not ready"; rc=1; }
assert_eq "$(mdsum "${LD}p1")" "$MD" "G 重跑后数据逐字节一致"
e2fsck -fn "${LD}p1" >/dev/null 2>&1 && echo "p1 e2fsck clean" || { echo "p1 e2fsck ISSUES"; rc=1; }
lo_detach "$LD"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }
exit $rc