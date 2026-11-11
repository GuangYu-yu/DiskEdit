#!/usr/bin/env bash
# 记录损坏 / --chunk 变更 / 大 chunk / align / 边界
# 去重归属：块设备路径（is_block=true / 4Kn）的唯一归属是 blockdev_4kn.sh；
#           move + chunk 注入续传的唯一归属是 resize_part_fault_paths.sh。
source "$(dirname "$0")/lib.sh"
require_fault_bin

img T t19.img


mk1() { # 1G: p1 100M p2 100M p3 200M
  rm -rf "$T" "$T"$SIDECAR_GLOB
  truncate -s 1G "$T"
  $B new "$T" --yes >/dev/null
  $B add "$T" 100M --align mib --name p1 --yes >/dev/null
  $B mkfs "$T":1 ext4 --yes >/dev/null
  $B add "$T" 100M --align mib --name p2 --yes >/dev/null
  $B mkfs "$T":2 ext4 --yes >/dev/null
  $B add "$T" 200M --align mib --name p3 --yes >/dev/null
  $B mkfs "$T":3 ext4 --yes >/dev/null
}

echo
echo "########## A: 记录损坏（截断/垃圾尾/magic 错）→ 不得误用 ##########"
for kind in trunc junk magic; do
  mk1
  LD=$(lo_attach "$T")
  mount_at "${LD}p2" "$TEST_MNT" && dd if=/dev/urandom of="$TEST_MNT/blob" bs=1M count=90 2>/dev/null; umount "$TEST_MNT"
  M2=$(mdsum "${LD}p2"); lo_detach "$LD"
  DISKEDIT_FAULT=chunk:40 $BF move "$T":2 --start 900000 --chunk 1 --yes >/dev/null 2>&1
  REC=$(record_file "$T")
  case $kind in
    trunc) head -c 40 "$REC" > "$REC.new" && mv "$REC.new" "$REC" ;;
    junk)  printf 'GARBAGE-TAIL-XXXX' >> "$REC" ;;
    magic) printf 'XXXX' | dd of="$REC" bs=1 seek=0 conv=notrunc 2>/dev/null ;;
  esac
  OUT=$($B move "$T":2 --start 900000 --chunk 1 --yes 2>&1); E=$?
  RS=$(echo "$OUT" | grep -oc "resuming at chunk")
  echo "[$kind] exit=$E resume_used=$RS"
  LD=$(lo_attach "$T")
  [ "$M2" = "$(mdsum "${LD}p2")" ] && echo "  p2 DEV OK" || { echo "  p2 DEV CORRUPT"; rc=1; }
  e2fsck -fn "${LD}p2" >/dev/null 2>&1 && echo "  p2 e2fsck clean" || { echo "  p2 e2fsck ISSUES"; rc=1; }
  lo_detach "$LD"
  sgdisk -v "$T" >/dev/null 2>&1 && echo "  sgdisk clean" || { echo "  sgdisk ISSUES"; rc=1; }
done
echo "说明：junk 尾不影响前段有效记录解析（原子 rename 下本不会产生半份记录），故可续传"

echo
echo "########## B: --chunk 变更 → 拒绝 ##########"
mk1
DISKEDIT_FAULT=chunk:30 $BF move "$T":2 --start 900000 --chunk 1 --yes >/dev/null 2>&1
OUT=$($B move "$T":2 --start 900000 --chunk 2 --yes 2>&1); E=$?
echo "exit=$E  msg: $(echo "$OUT" | grep -o 'does not match.*' | head -1)"
OUT=$($B move "$T":2 --start 900000 --chunk 1 --yes 2>&1)
echo "同参数重跑: $(echo "$OUT" | grep -o 'resuming at chunk [0-9]*')"

echo
echo "########## C: 大 chunk（64 MiB，取值上界）##########"
mk1
DISKEDIT_FAULT=chunk:1 $BF move "$T":2 --start 900000 --chunk 64 --yes >/dev/null 2>&1
OUT=$($B move "$T":2 --start 900000 --chunk 64 --yes 2>&1)
echo "resume: $(echo "$OUT" | grep -o 'resuming at chunk [0-9]*')  (分区 100M > chunk 64M → 两块)"
LD=$(lo_attach "$T")
mount_at "${LD}p2" "$TEST_MNT" && dd if=/dev/urandom of="$TEST_MNT/b2" bs=1M count=20 2>/dev/null && umount "$TEST_MNT"
lo_detach "$LD"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "########## D: --align ##########"
mk1
$B move "$T":2 --start 900001 --align none --chunk 4 --yes >/dev/null 2>&1
echo "none: $($B info "$T" | grep -o '"num":2,"first_lba":[0-9]*')  (期望 first_lba=900001)"
mk1
$B move "$T":2 --start 900001 --align mib --chunk 4 --yes >/dev/null 2>&1
echo "mib : $($B info "$T" | grep -o '"num":2,"first_lba":[0-9]*')  (期望 901120 = 440×2048)"

echo
echo "########## E: 边界（128 分区上限） ##########"
img T2 t19b.img; T=$T2
rm -rf "$T" "$T"$SIDECAR_GLOB
truncate -s 1G "$T"
$B new "$T" --yes >/dev/null
OK=0
for i in $(seq 1 129); do
  if $B add "$T" 1M --align mib --yes >/dev/null 2>&1; then OK=$((OK+1)); else break; fi
done
echo "added $OK partitions (上限 128)"
[ "$OK" -le 128 ] && echo "BOUND OK (≤128)" || { echo "BOUND FAIL (>128)"; rc=1; }
$B info "$T" | grep -o '"num":[0-9]*' | wc -l
exit $rc