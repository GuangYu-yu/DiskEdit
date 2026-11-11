#!/usr/bin/env bash
# resize_part 路径（move / 收缩 / 扩容收尾 / grow）+ 注入；copy 中断
source "$(dirname "$0")/lib.sh"
require_fault_bin

img T t18a.img

mk() {
  rm -rf "$T" "$T"$SIDECAR_GLOB
  truncate -s 1G "$T"
  $BF new "$T" --yes >/dev/null
  $BF add "$T" 100M --align mib --name p1 --yes >/dev/null
  $BF mkfs "$T":1 ext4 --yes >/dev/null 2>&1
  $BF add "$T" 100M --align mib --name p2 --yes >/dev/null
  $BF mkfs "$T":2 ext4 --yes >/dev/null 2>&1
  $BF add "$T" 200M --align mib --name p3 --yes >/dev/null
  $BF mkfs "$T":3 ext4 --yes >/dev/null 2>&1
}
fillp() { # part MB -> 整分区设备 md5
  LD=$(lo_attach "$T")
  lo_waitpart "${LD}p$1" || { echo "p$1 part node not ready"; rc=1; lo_detach "$LD"; return 1; }
  mount_at "${LD}p$1" "$TEST_MNT" && dd if=/dev/urandom of="$TEST_MNT/blob" bs=1M count=$2 2>/dev/null
  umount "$TEST_MNT"; mdsum "${LD}p$1"; lo_detach "$LD"
}
chkp() { # part want-md5（传 "-" 则跳过哈希对比，仅 fsck；FS 扩缩后设备哈希必变）
  LD=$(lo_attach "$T") || return 1
  lo_waitpart "${LD}p$1" || { echo "p$1 part node not ready"; rc=1; lo_detach "$LD"; return 1; }
  if [ "$2" != "-" ]; then
    if [ "$2" = "$(mdsum "${LD}p$1")" ]; then
      echo "p$1 DEV OK"
    else
      echo "p$1 DEV CORRUPT (size=$(blockdev --getsize64 "${LD}p$1" 2>/dev/null))"; rc=1
    fi
  fi
  local fsck_out
  if fsck_out=$(e2fsck -fn "${LD}p$1" 2>&1); then
    echo "p$1 e2fsck clean"
  else
    echo "p$1 e2fsck ISSUES (exit=$?)"; echo "$fsck_out" | tail -5; rc=1
  fi
  lo_detach "$LD"
}

echo "===== 1: move + chunk:60 ====="
mk; M2=$(fillp 2 90)
DISKEDIT_FAULT=chunk:60 $BF move "$T":2 --start 900000 --chunk 1 --yes >/dev/null 2>&1
OUT=$($BF move "$T":2 --start 900000 --chunk 1 --yes 2>&1)
# move 是 chunk 形续跑：打印不带 entry 前缀
R=$(echo "$OUT" | grep -o "resuming at chunk [0-9]*" | head -1)
N=$(echo "$R" | sed 's/.*chunk //')
# 非自重叠搬移按 flush_at = max(chunk, 8MiB) 落进度：崩溃后 durable 游标停在最近一次
# 提交点上（1M chunk 即每 8 块一提交），必然大于 0 且不大于中断块号
if [ -n "$N" ] && [ "$N" -gt 0 ] && [ "$N" -le 60 ]; then
  echo "RS-RESUME OK ($R)"
else
  echo "RS-RESUME WRONG ($R)"; rc=1
fi
chkp 2 "$M2"

echo
echo "===== 2: before-entry-commit ====="
mk; M2=$(fillp 2 90)
# move 的表提交前中断走 before-entry-commit：数据已搬完、表未动
DISKEDIT_FAULT=before-entry-commit $BF move "$T":2 --start 900000 --chunk 1 --yes >/dev/null 2>&1
OUT=$($BF move "$T":2 --start 900000 --chunk 1 --yes 2>&1)
R=$(echo "$OUT" | grep -o "resuming at chunk [0-9]*" | head -1)
[ -n "$R" ] && echo "RS-PRECOMMIT OK ($R)" || { echo "RS-PRECOMMIT WRONG"; rc=1; }
chkp 2 "$M2"

echo
echo "===== 3: rs-after-commit（扩容收尾未做）→ rerun 续做收尾 ====="
mk; M3=$(fillp 3 190)
# p3 200M → 300M：起点 411648，末端 = 411648 + 614400 − 1 = 1026047
DISKEDIT_FAULT=rs-after-commit $BF resize-part --yes "$T":3 --end 1026047 >/dev/null 2>&1
echo "record: $(record_present "$T" && echo present || echo absent)"
# 表已在首次运行中提交：同请求 rerun 沿记录续做 FS 收尾，不再重写表
OUT=$($BF resize-part --yes "$T":3 --end 1026047 2>&1); E=$?
echo "$OUT" | grep -o "resize already committed.*" | head -1
exp "$E" 0 "表已提交后 rerun 续做 FS 收尾（exit 0）"
LD=$(lo_attach "$T")
lo_waitpart "${LD}p3" || { echo "p3 part node not ready"; rc=1; lo_detach "$LD"; }
BC=$(dumpe2fs -h "${LD}p3" 2>/dev/null | grep -i '^Block count' | tr -dc '0-9')
exp "$BC" 76800 "resizefs 补齐扩容收尾（300M）"
lo_detach "$LD"
$B info "$T" | grep -o '"num":3,"first_lba":[0-9]*,"last_lba":[0-9]*'
chkp 3 -

echo
echo "===== 4: 缩容 + rs-after-fs-shrink → FS 已缩、表未改 → 同请求重跑续做 ====="
# resize-part 的缩容把 FS 步与收表放进同一份记录：FS 缩完即落进度（fs_shrunk=Verified），
# 中断后同请求重跑跳过已缩的 FS 步、只收表
mk; M1=$(fillp 1 20)
DISKEDIT_FAULT=rs-after-fs-shrink $BF resize-part --yes "$T":1 --end 145407 >/dev/null 2>&1
echo "record: $(record_present "$T" && echo present || echo absent)"
OUT=$($BF resize-part --yes "$T":1 --end 145407 2>&1); exp "$?" 0 "同请求重跑续做缩容"
$B info "$T" | grep -o '"num":1,"first_lba":[0-9]*,"last_lba":[0-9]*' | grep -q '"last_lba":145407' \
  && echo "  OK   p1 收缩到 70M（last_lba=145407）" || { echo "  BAD  p1 未收缩到预期终点"; rc=1; }
chkp 1 -

echo
# `resize-part --start` 列为未登记，resize-part 只能改终点，不产生 chunk 搬移，
# chunk 注入标签在该命令上不可达；搬移中断的覆盖在同文件的 1/2/6 段（move / resize）。

echo
echo "===== 6: grow 路径（尾打包）chunk:150 ====="
mk; M2=$(fillp 2 90); M3=$(fillp 3 190)
DISKEDIT_FAULT=chunk:150 $BF resize "$T":1 grow --allow-move --yes --chunk 1 >/dev/null 2>&1
OUT=$($BF resize "$T":1 grow --allow-move --yes --chunk 1 2>&1)
R=$(echo "$OUT" | grep -o "resuming at entry [0-9]* chunk [0-9]*" | head -1)
N=$(echo "$R" | sed 's/.*chunk //')
echo "resume: $R (checkpoint 按 flush_at 落盘，只断言续跑点落在已提交边界上)"
if [ -n "$N" ] && [ "$N" -gt 0 ] && [ "$N" -le 150 ]; then
  echo "  RESUME OK ($R)"
else
  echo "  RESUME WRONG ($R)"; rc=1
fi
chkp 2 "$M2"; chkp 3 "$M3"

echo
echo "===== 7: copy 确定性中断（copy-before-commit）→ 重跑拒 → abandon → 重跑 → 数据一致 ====="
img T2 t18b.img; T=$T2
mk; M3=$(fillp 3 190)
# 数据已拷、表项尚未提交的确定点：copy 不可续传，现场只能靠 abandon 释放。
# 复制已写盘（记录 provisional）：重跑按 past-boundary 口径拒 5，消息仍指向在用作业
DISKEDIT_FAULT=copy-before-commit $BF copy "$T":3 --start 1500000 --chunk 1 --yes >/dev/null 2>&1
exp "$(record_present "$T" && echo yes || echo no)" yes "copy 中断留下记录现场（不可续传，下面两行验证 rerun 硬拒）"
$B info "$T" | grep -q '"num":4' && { echo "  BAD  p4 已进表（应在提交前中断）"; rc=1; } || echo "  OK   p4 尚未进表"
OUT=$($BF copy "$T":3 --start 1500000 --chunk 1 --yes 2>&1); exp "$?" 5 "未释放现场时重跑被拒（past-boundary 口径）"
echo "$OUT" | grep -o "unfinished operation still owns this target" | head -1
exp "$($B abandon "$T" >/dev/null 2>&1; echo $?)" 0 "abandon 释放 copy 现场"
OUT=$($B copy "$T":3 --start 1500000 --chunk 1 --yes 2>&1); exp "$?" 0 "abandon 后重跑 copy"
$B info "$T" | grep -q '"num":4' && echo "  OK   p4 已落表" || { echo "  BAD  p4 未进表"; rc=1; }
chkp 3 "$M3"
LD=$(lo_attach "$T")
lo_waitpart "${LD}p4" || { echo "p4 part node not ready"; rc=1; }
[ "$M3" = "$(mdsum "${LD}p4")" ] && echo "p4 COPY OK (device-identical)" || { echo "p4 COPY MISMATCH"; rc=1; }
lo_detach "$LD"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "===== 8: after-data-sync（自重叠搬移，第 1 块同步后、进度落盘前中断）====="
# 数据同步与持久进度之间的交界面只在自重叠搬移上逐块存在：overlap 分支每块
# sync_data 之后先注入再落进度（execution/relocate.rs）。非自重叠按
# flush_at = max(chunk_bytes, CHECKPOINT_FLOOR = 8 MiB) 攒批，批内没有独立注入点。
# 注入点取第 1 块（游标 = 1 且尚未落盘）：记录里的游标仍是开工时写下的 0
# → 同请求重跑从 chunk 0 继续
img T t18c.img
mk; M3=$(fillp 3 190)
# p3（411648..821247）右移到 700000：目标落在尾部空闲区、与源交叠 → 自重叠右移；
# p2 两侧被 p1/p3 夹死，move 单分区在任何方向都撞邻居，几何上不存在可搬的窗口
DISKEDIT_FAULT=after-data-sync:1 $BF move "$T":3 --start 700000 --chunk 1 --yes >/dev/null 2>&1
exp "$(record_present "$T" && echo yes || echo no)" yes "中断留下记录现场"
OUT=$($BF move "$T":3 --start 700000 --chunk 1 --yes 2>&1); exp "$?" 0 "同请求重跑搬移"
R=$(echo "$OUT" | grep -o "resuming at chunk [0-9]*" | head -1)
[ "$R" = "resuming at chunk 0" ] && echo "  AFTER-SYNC OK ($R)" || { echo "  AFTER-SYNC WRONG ($R)"; rc=1; }
chkp 3 "$M3"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "===== 9: Δ<0 自重叠（64M 分区左移 16M，--chunk 64 收敛到 16M）====="
# 起点 67584（33 MiB）左侧留出 16 MiB 位移；终点 198655。位移 32768 扇区小于源长
# 131072 扇区 → 自重叠，块大小收敛到 min(64M, 16M) = 16M，共 4 块。
# 搬完按区间 sha256 与挂载读回双重校验；中断点取第 2 块，记录游标与续跑点都可观测
ranged_sha() { dd if="$1" bs=512 skip="$2" count="$3" status=none | sha256sum | cut -d' ' -f1; }
img T t18d.img
rm -rf "$T" "$T"$SIDECAR_GLOB
truncate -s 256M "$T"
$BF new "$T" --yes >/dev/null
$BF add "$T" 64M --start 67584 --align none --name left --yes >/dev/null
exp "$?" 0 "add 64M @67584"
$BF mkfs "$T":1 ext4 --yes >/dev/null 2>&1
exp "$?" 0 "mkfs ext4"
LD=$(lo_attach "$T")
lo_waitpart "${LD}p1" || { echo "  BAD  p1 未就绪"; rc=1; }
mount_at "${LD}p1" "$TEST_MNT" || { echo "  BAD  挂载失败"; rc=1; }
dd if=/dev/urandom of="$TEST_MNT/blob" bs=1M count=48 2>/dev/null
BLOB=$(sha256sum "$TEST_MNT"/blob | cut -d' ' -f1)
unmount_now "$TEST_MNT"
lo_detach "$LD"
BEFORE=$(ranged_sha "$T" 67584 131072)
DISKEDIT_FAULT=chunk:2 $BF move "$T":1 --start 34816 --chunk 64 --yes >/dev/null 2>&1
exp "$(record_present "$T" && echo yes || echo no)" yes "第 2 块落盘后中断留下记录现场"
C=$(record_cursor "$T")
assert_eq "$C" "2" "记录里的搬移游标停在第 2 块"
OUT=$($BF move "$T":1 --start 34816 --chunk 64 --yes 2>&1); exp "$?" 0 "同请求重跑搬移"
R=$(echo "$OUT" | grep -o "resuming at chunk [0-9]*" | head -1)
assert_eq "$R" "resuming at chunk 2" "续跑点取记录里的游标"
$B info "$T" | grep -q '"num":1,"first_lba":34816' && echo "  OK   p1 到达 34816" || { echo "  BAD  p1 未到达目标起点"; rc=1; }
AFTER=$(ranged_sha "$T" 34816 131072)
assert_eq "$AFTER" "$BEFORE" "左移后源数据逐字节一致（分区内容 sha256）"
LD=$(lo_attach "$T")
lo_waitpart "${LD}p1" || { echo "  BAD  p1 未就绪"; rc=1; }
mount_at "${LD}p1" "$TEST_MNT" || { echo "  BAD  挂载失败"; rc=1; }
assert_eq "$(sha256sum "$TEST_MNT"/blob | cut -d' ' -f1)" "$BLOB" "移动后挂载读回的 blob 一致"
unmount_now "$TEST_MNT"
e2fsck -fn "${LD}p1" >/dev/null 2>&1 && echo "  OK   p1 e2fsck clean" || { echo "  BAD  p1 e2fsck 报错"; rc=1; }
lo_detach "$LD"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "===== 10: 块收敛（--chunk 64 > |Δ|16M ⇒ 单块 16M，共 4 块）====="
# 共 4 块：第 4 块的注入必须命中、第 5 块必须不存在。若命令行给的 64M 被直接采用
# 就只有 1 块，两处断言都会失败
mk_left() {
  rm -rf "$1" "$1"$SIDECAR_GLOB
  truncate -s 256M "$1"
  $BF new "$1" --yes >/dev/null
  $BF add "$1" 64M --start 67584 --align none --name left --yes >/dev/null
}
img T t18e.img
mk_left "$T"
DISKEDIT_FAULT=chunk:4 $BF move "$T":1 --start 34816 --chunk 64 --yes >/dev/null 2>&1
exp "$(record_present "$T" && echo yes || echo no)" yes "第 4 块存在（单块被压到 16M）"
C=$(record_cursor "$T")
assert_eq "$C" "4" "第 4 块后游标为 4"
OUT=$($BF move "$T":1 --start 34816 --chunk 64 --yes 2>&1); exp "$?" 0 "续跑完成"
R=$(echo "$OUT" | grep -o "resuming at chunk [0-9]*" | head -1)
assert_eq "$R" "resuming at chunk 4" "续跑点 = 4（最后一块）"
mk_left "$T"
DISKEDIT_FAULT=chunk:5 $BF move "$T":1 --start 34816 --chunk 64 --yes >/dev/null 2>&1
exp "$?" 0 "第 5 块不存在（命令行 chunk 64M 未被采用）"
exp "$(record_absent "$T" && echo yes || echo no)" yes "正常跑完，无残留记录"
$B info "$T" | grep -q '"num":1,"first_lba":34816' && echo "  OK   p1 到达 34816" || { echo "  BAD  p1 未到达目标起点"; rc=1; }
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }
exit $rc