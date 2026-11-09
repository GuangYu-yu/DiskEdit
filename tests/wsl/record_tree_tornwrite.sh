#!/usr/bin/env bash
# 单文件记录流式（数据字节不入记录）/ FS 深度校验 / 复杂目录树 / torn write 模拟
# 残留控制：所有产物用 t22 前缀，trap EXIT 强制清理
source "$(dirname "$0")/lib.sh"
require_fault_bin



img T t22a.img
SNAP=$WSL_ROOT/t22snap

mk() { # 100M/100M/200M，p2 前后留空便于自重叠左移
  rm -rf "$T" "$T"$SIDECAR_GLOB
  truncate -s 1G "$T"
  $B new "$T" --yes >/dev/null
  $B add --yes "$T" 104857600 --start 2048 --name p1 >/dev/null && $B mkfs "$T":1 ext4 --yes >/dev/null
  $B add --yes "$T" 104857600 --start 411648 --name p2 >/dev/null && $B mkfs "$T":2 ext4 --yes >/dev/null
  $B add --yes "$T" 209715200 --start 1026048 --name p3 >/dev/null && $B mkfs "$T":3 ext4 --yes >/dev/null
}
mnt() {
  LD=$(lo_attach "$T") || return 1
  if [ -n "$1" ]; then
    lo_waitpart "${LD}p$1" || { lo_detach "$LD"; return 1; }
    mount_at "${LD}p$1" "$TEST_MNT" || { lo_detach "$LD"; return 1; }
  fi
  return 0
}
umnt() { umount "$TEST_MNT" 2>/dev/null; lo_detach "$LD"; }

echo "########## A: 记录流式（数据字节不入记录） ##########"
mk
mnt 2; dd if=/dev/urandom of="$TEST_MNT/blob" bs=1M count=90 2>/dev/null; umnt; LD=$(lo_attach "$T"); MD2=$(mdsum "${LD}p2"); lo_detach "$LD"
echo "-- A1: 中断的 100M 分区搬移后记录体积 --"
OUT=$(DISKEDIT_FAULT=chunk:60 $BF move "$T":2 --start 250000 --chunk 1 --yes 2>&1); E=$?
[ "$E" = "134" ] || { echo "  BAD  中断未按预期触发（exit=$E）"; echo "$OUT" | head -2; rc=1; }
JSZ=$(stat -c%s "$(record_file "$T")" 2>/dev/null || echo 0)
echo "record = $JSZ bytes (搬移量约 100M)"
if [ "$JSZ" -gt 5 ] && [ "$JSZ" -lt 1048576 ]; then echo "A1 RECORD-STREAMING OK (<1MiB, 只记进度不记数据)"; else { echo "A1 JOURNAL SIZE ODD ($JSZ)"; rc=1; }; fi
echo "-- A2: 中断搬移的 undo——表区回放、数据不回滚--"
# 表步骤未提交，preimage（只覆盖表区）与盘面一致 → 无字节可写，按 complete 收尾退 0；
# 已搬走的数据留在落点上，undo 从不回收它
OUT=$($B undo "$T" --yes 2>&1); E=$?
echo "exit=$E msg: $(echo "$OUT" | head -1 | cut -c1-70)"
[ "$E" = "0" ] && echo "$OUT" | grep -q "nothing was written back" && echo "A2 UNDO-TABLE-ONLY OK" \
  || { echo "A2 UNDO WRONG (want 0 + nothing was written back)"; rc=1; }
echo "-- A3: 完成后记录应被删除 --"
$B move "$T":2 --start 250000 --chunk 1 --yes >/dev/null 2>&1; E=$?
record_absent "$T" && echo "A3 RECORD DROPPED OK (exit=$E)" || { echo "A3 RECORD LEFT (bad)"; rc=1; }
LD=$(lo_attach "$T")
[ "$MD2" = "$(mdsum "${LD}p2")" ] && echo "A3 p2 DEV OK" || { echo "A3 p2 DEV CORRUPT"; rc=1; }
e2fsck -fn "${LD}p2" >/dev/null 2>&1 && echo "A3 e2fsck clean" || { echo "A3 e2fsck ISSUES"; rc=1; }
lo_detach "$LD"

echo
echo "########## B: 复杂目录树 + 自重叠搬移 + e2fsck 深校验 ##########"
mk
mnt 2 || { echo "BAD  B 段挂载 p2 失败"; rc=1; }
mkdir -p "$TEST_MNT"/a/b/c "$TEST_MNT"/d
for i in $(seq 1 300); do echo "content-$i-abc" > "$TEST_MNT/a/f$i.txt"; done
for i in $(seq 1 100); do echo "d-$i" > "$TEST_MNT/d/g$i"; done
ln -s a/f1.txt "$TEST_MNT"/link1
ln "$TEST_MNT"/a/f2.txt "$TEST_MNT"/hardlink1
dd if=/dev/urandom of="$TEST_MNT/a/rand.bin" bs=1M count=30 2>/dev/null
(cd "$TEST_MNT" && find . -type f -exec md5sum {} \; | sort) > "$SNAP.1"
(cd "$TEST_MNT" && find . | sort) > "$SNAP.t1"
NF1=$(wc -l < "$SNAP.1")
echo "files: $NF1, symlink: $(readlink "$TEST_MNT"/link1), hardlink count: $(stat -c%h "$TEST_MNT"/a/f2.txt)"
umnt
# 自重叠左移：delta = 249856-411648 = -161792，|delta| < 长度 204800
$B move "$T":2 --start 250000 --chunk 1 --yes >/dev/null 2>&1; echo "move exit=$?"
LD=$(lo_attach "$T")
e2fsck -fn "${LD}p2" > "$WSL_ROOT/t22e2out" 2>&1; EC=$?
echo "e2fsck -fn exit=$EC (0=clean)"
tail -2 "$WSL_ROOT/t22e2out"
mount_at "${LD}p2" "$TEST_MNT"
(cd "$TEST_MNT" && find . -type f -exec md5sum {} \; | sort) > "$SNAP.2"
(cd "$TEST_MNT" && find . | sort) > "$SNAP.t2"
# 内容一致之外还要足量：快照若因建树失败只剩零星文件，diff 相等会造出假阳性
NF2=$(wc -l < "$SNAP.2")
if diff -q "$SNAP.1" "$SNAP.2" >/dev/null && [ "$NF1" -ge 400 ] && [ "$NF2" -ge 400 ]; then
  echo "B FILE-CONTENTS OK ($NF2 files)"
else
  echo "B FILE CONTENTS DIFFER OR INCOMPLETE ($NF1 -> $NF2)"; rc=1; diff "$SNAP.1" "$SNAP.2" | head -5;
fi
diff -q "$SNAP.t1" "$SNAP.t2" >/dev/null && echo "B TREE OK" || { echo "B TREE DIFFERS"; rc=1; }
[ "$(readlink "$TEST_MNT"/link1)" = "a/f1.txt" ] && echo "B SYMLINK OK" || { echo "B SYMLINK BROKEN"; rc=1; }
[ "$(stat -c%h "$TEST_MNT"/a/f2.txt)" -ge 2 ] && echo "B HARDLINK OK" || { echo "B HARDLINK BROKEN"; rc=1; }
umnt

echo
echo "########## C: torn write 模拟（GPT 头损坏 / 备份头损坏） ##########"
mk
echo "-- C1: 擦除主头（LBA1）后读表 --"
dd if=/dev/zero of="$T" bs=512 seek=1 count=1 conv=notrunc 2>/dev/null
OUT=$($B info "$T" 2>&1)
if echo "$OUT" | grep -q '"label":"none"'; then
  echo "C1 GAP: 主头损坏 → 工具视为无表（不回退备份头）"
else
  echo "C1 OK: 从备份头读到表"
fi
echo "-- C2: 主头损坏时写操作（工具已从备份头恢复表 → 写后应自洽）--"
$B resize-part --yes "$T":3 --grow-to-end >/dev/null 2>&1; E=$?
echo "write exit=$E"
sgdisk -v "$T" >/dev/null 2>&1 && echo "C2 RECOVERED-CLEAN OK" || { echo "C2 INCONSISTENT AFTER WRITE"; rc=1; }
echo "-- C3: 主头损坏 + new → 备份头被覆盖？（数据丢失风险） --"
$B new "$T" --yes >/dev/null 2>&1; echo "new exit=$?"
N=$($B info "$T" | grep -o '"num":' | wc -l)
echo "new 后可见分区数: $N"
sgdisk -v "$T" 2>&1 | tail -1
echo "-- C4: 擦除备份头（盘尾）→ 主头可用则不受影响 --"
mk
SZ=$(stat -c%s "$T"); LAST=$((SZ / 512 - 1))
dd if=/dev/zero of="$T" bs=512 seek=$LAST count=1 conv=notrunc 2>/dev/null
echo "info: $($B info "$T" | head -c 50)"
$B resize-part --yes "$T":3 --grow-to-end >/dev/null 2>&1; echo "repair exit=$?"
$B info "$T" | grep -o '"num":3,[^}]*}'
sgdisk -v "$T" 2>&1 | tail -1

echo "-- C5: 保护 MBR SizeInLBA 超出容器（Inconsistent）→ 拒绝自动修复 --"
# 依据：src/table/gpt_parse.rs:525 pmbr_size_state：size > 容器-1（两种口径都大于）→ Inconsistent；
# src/gpt_policy.rs:93-99 classify_repair 对 Inconsistent 返回 Err（拒绝自动修复，可能是更大盘的
# 截断副本），经 src/gpt_policy.rs:203 归 Fail::refused → 退出码 3（表结构非法，未写盘）；
# info 走 load_gpt（不做修复）仍可读，并把提示打入 stderr（src/cmd/info.rs:110-118）
mk
printf '\xff\xff\xff\xff' | dd of="$T" bs=1 seek=458 conv=notrunc 2>/dev/null
OUT=$($B info "$T" 2>&1); E=$?
echo "info exit=$E : $(echo "$OUT" | grep -o 'refusing auto-repair[^"]*' | head -1)"
if [ "$E" = "0" ] && echo "$OUT" | grep -q "refusing auto-repair"; then
  echo "C5 INFO-REPORTS OK"
else
  echo "C5 INFO WRONG (exit=$E)"; rc=1
fi
OUT=$($B set --yes "$T":1 flag esp on 2>&1); E=$?
echo "write exit=$E : $(echo "$OUT" | tail -1 | cut -c1-90)"
if [ "$E" = "3" ] && echo "$OUT" | grep -q "refusing auto-repair"; then
  echo "C5 INCONSISTENT-REFUSED OK"
else
  echo "C5 WRONG EXIT (exit=$E, want 3)"; rc=1
fi

echo
echo "########## 残留核对 ##########"
[ ! -e "$WSL_ROOT" ] && echo "root gone"; ls "$WSL_ROOT" 2>/dev/null | wc -l
ls "$WSL_ROOT" 2>/dev/null | wc -l
losetup -a | wc -l
exit $rc