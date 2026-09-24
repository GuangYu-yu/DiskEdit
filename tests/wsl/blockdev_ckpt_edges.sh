#!/usr/bin/env bash
# ckpt 损坏 / chunk-size 变更 / 大 chunk / align / 边界
# 去重归属：块设备路径（is_block=true / 4Kn）的唯一归属是 blockdev_4kn.sh；
#           move + rs-chunk 续传的唯一归属是 resize_part_fault_paths.sh。
#           原"块设备 move + rs-chunk:50 中断"用例与上述两处语义重复（且续传断言更弱），已删。
source "$(dirname "$0")/lib.sh"
require_fault_bin

T=/var/tmp/t19.img

cleanup_hook() {
  local f
  for f in /var/tmp/t19.img /var/tmp/t19b.img; do
    rm -f "$f" "$f".diskedit.* 2>/dev/null
  done
  rm -f /var/lib/diskedit/*.ckpt 2>/dev/null
}

track_file "$T"

mk1() { # 1G: p1 100M p2 100M p3 200M
  rm -f "$T" "$T".diskedit.*; rm -f /var/lib/diskedit/*.ckpt
  truncate -s 1G "$T"
  $B new "$T" --yes >/dev/null
  $B create "$T" --size 100M --name p1 --fs ext4 >/dev/null
  $B create "$T" --size 100M --name p2 --fs ext4 >/dev/null
  $B create "$T" --size 200M --name p3 --fs ext4 >/dev/null
}

echo
echo "########## A: ckpt 损坏（截断/垃圾尾/magic 错）→ 不得误用 ##########"
for kind in trunc junk magic; do
  mk1
  LD=$(lo_attach "$T")
  mount_at "${LD}p2" /testmnt && dd if=/dev/urandom of=/testmnt/blob bs=1M count=90 2>/dev/null; umount /testmnt
  M2=$(md5sum "${LD}p2" | cut -d' ' -f1); lo_detach "$LD"
  DISKEDIT_FAULT=rs-chunk:40 $BF move "$T":2 --start 900000 --chunk-size 1 >/dev/null 2>&1
  case $kind in
    trunc) head -c 40 "$T.diskedit.ckpt" > "$T.ckpt.new" && mv "$T.ckpt.new" "$T.diskedit.ckpt" ;;
    junk)  printf 'GARBAGE-TAIL-XXXX' >> "$T.diskedit.ckpt" ;;
    magic) printf 'XXXX' | dd of="$T.diskedit.ckpt" bs=1 seek=0 conv=notrunc 2>/dev/null ;;
  esac
  OUT=$($B move "$T":2 --start 900000 --chunk-size 1 2>&1); E=$?
  RS=$(echo "$OUT" | grep -oc "resuming at chunk")
  echo "[$kind] exit=$E resume_used=$RS"
  LD=$(lo_attach "$T")
  [ "$M2" = "$(md5sum "${LD}p2" | cut -d' ' -f1)" ] && echo "  p2 DEV OK" || { echo "  p2 DEV CORRUPT"; rc=1; }
  e2fsck -fn "${LD}p2" >/dev/null 2>&1 && echo "  p2 e2fsck clean" || { echo "  p2 e2fsck ISSUES"; rc=1; }
  lo_detach "$LD"
  sgdisk -v "$T" >/dev/null 2>&1 && echo "  sgdisk clean" || { echo "  sgdisk ISSUES"; rc=1; }
done
echo "说明：junk 尾不影响前段有效记录解析（原子 rename 下本不会产生半份 ckpt），故可续传"

echo
echo "########## B: chunk-size 变更 → 拒绝 ##########"
mk1
DISKEDIT_FAULT=rs-chunk:30 $BF move "$T":2 --start 900000 --chunk-size 1 >/dev/null 2>&1
OUT=$($B move "$T":2 --start 900000 --chunk-size 2 2>&1); E=$?
echo "exit=$E  msg: $(echo "$OUT" | grep -o 'does not match.*' | head -1)"
OUT=$($B move "$T":2 --start 900000 --chunk-size 1 2>&1)
echo "同参数重跑: $(echo "$OUT" | grep -o 'resuming at chunk [0-9]*')"

echo
echo "########## C: 大 chunk（512 MiB）##########"
mk1
DISKEDIT_FAULT=rs-chunk:1 $BF move "$T":2 --start 900000 --chunk-size 512 >/dev/null 2>&1
OUT=$($B move "$T":2 --start 900000 --chunk-size 512 2>&1)
echo "resume: $(echo "$OUT" | grep -o 'resuming at chunk [0-9]*')  (分区 100M < chunk 512M → 单 chunk)"
LD=$(lo_attach "$T")
mount_at "${LD}p2" /testmnt && dd if=/dev/urandom of=/testmnt/b2 bs=1M count=20 2>/dev/null && umount /testmnt
lo_detach "$LD"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "########## D: --align ##########"
mk1
$B move "$T":2 --start 900001 --align none --chunk-size 4 >/dev/null 2>&1
echo "none: $($B info "$T" | grep -o '"num":2,"first_lba":[0-9]*')  (期望 first_lba=900001)"
mk1
$B move "$T":2 --start 900001 --align cyl --chunk-size 4 >/dev/null 2>&1
echo "cyl : $($B info "$T" | grep -o '"num":2,"first_lba":[0-9]*')  (期望 915705 = 57×16065)"
mk1
$B move "$T":2 --start 900001 --align mib --chunk-size 4 >/dev/null 2>&1
echo "mib : $($B info "$T" | grep -o '"num":2,"first_lba":[0-9]*')  (期望 901120 = 440×2048)"

echo
echo "########## E: 边界（128 分区上限） ##########"
T2=/var/tmp/t19b.img; T=$T2
rm -f "$T" "$T".diskedit.*
truncate -s 1G "$T"
$B new "$T" --yes >/dev/null
OK=0
for i in $(seq 1 129); do
  if $B create "$T" --size 1M >/dev/null 2>&1; then OK=$((OK+1)); else break; fi
done
echo "created $OK partitions (上限 128)"
[ "$OK" -le 128 ] && echo "BOUND OK (≤128)" || { echo "BOUND FAIL (>128)"; rc=1; }
$B info "$T" | grep -o '"num":[0-9]*' | wc -l
exit $rc