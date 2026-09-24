#!/usr/bin/env bash
# resize_part 路径（move / 收缩 / 扩容收尾 / grow）+ 注入；copy 中断
source "$(dirname "$0")/lib.sh"
require_fault_bin

T=/var/tmp/t18a.img

cleanup_hook() { rm -f /var/tmp/t18*.img* 2>/dev/null; }

rm -f /var/tmp/t18*.img* 2>/dev/null
track_file "$T"

mk() {
  rm -f "$T" "$T".diskedit.*
  truncate -s 1G "$T"
  $BF new "$T" --yes >/dev/null
  $BF create "$T" --size 100M --name p1 --fs ext4 >/dev/null
  $BF create "$T" --size 100M --name p2 --fs ext4 >/dev/null
  $BF create "$T" --size 200M --name p3 --fs ext4 >/dev/null
}
fillp() { # part MB -> 整分区设备 md5
  LD=$(lo_attach "$T")
  mount_at "${LD}p$1" /testmnt && dd if=/dev/urandom of=/testmnt/blob bs=1M count=$2 2>/dev/null
  umount /testmnt; md5sum "${LD}p$1" | cut -d' ' -f1; lo_detach "$LD"
}
chkp() { # part want-md5（传 "-" 则跳过哈希对比，仅 fsck；FS 扩缩后设备哈希必变）
  LD=$(lo_attach "$T") || return 1
  if [ "$2" != "-" ]; then
    [ "$2" = "$(md5sum "${LD}p$1" | cut -d' ' -f1)" ] && echo "p$1 DEV OK" || { echo "p$1 DEV CORRUPT"; rc=1; }
  fi
  e2fsck -fn "${LD}p$1" >/dev/null 2>&1 && echo "p$1 e2fsck clean" || { echo "p$1 e2fsck ISSUES"; rc=1; }
  lo_detach "$LD"
}

echo "===== 1: move + rs-chunk:60 ====="
mk; M2=$(fillp 2 90)
DISKEDIT_FAULT=rs-chunk:60 $BF move "$T":2 --start 900000 --chunk-size 1 >/dev/null 2>&1
OUT=$($BF move "$T":2 --start 900000 --chunk-size 1 2>&1)
R=$(echo "$OUT" | grep -o "resuming at chunk [0-9]*" | head -1)
[ "$R" = "resuming at chunk 60" ] && echo "RS-RESUME OK ($R)" || { echo "RS-RESUME WRONG ($R)"; rc=1; }
chkp 2 "$M2"

echo
echo "===== 2: rs-before-commit ====="
mk; M2=$(fillp 2 90)
DISKEDIT_FAULT=rs-before-commit $BF move "$T":2 --start 900000 --chunk-size 1 >/dev/null 2>&1
OUT=$($BF move "$T":2 --start 900000 --chunk-size 1 2>&1)
R=$(echo "$OUT" | grep -o "resuming at chunk [0-9]*" | head -1)
[ "$R" = "resuming at chunk 100" ] && echo "RS-PRECOMMIT OK ($R)" || { echo "RS-PRECOMMIT WRONG ($R)"; rc=1; }
chkp 2 "$M2"

echo
echo "===== 3: rs-after-commit（扩容收尾未做）→ 重跑应补做 FS 扩容 ====="
mk; M3=$(fillp 3 190)
DISKEDIT_FAULT=rs-after-commit $BF resize "$T":3 300M >/dev/null 2>&1
echo "ckpt left: $([ -f "$T.diskedit.ckpt" ] && echo yes || echo no)"
OUT=$($BF resize "$T":3 300M 2>&1); E=$?
echo "rerun exit=$E"
echo "$OUT" | grep -o "already committed.*"
echo "ckpt after: $([ -f "$T.diskedit.ckpt" ] && echo yes || echo no)"
LD=$(lo_attach "$T")
echo "p3 FS blocks: $(dumpe2fs -h "${LD}p3" 2>/dev/null | grep -i '^Block count' | tr -d ' ')"
lo_detach "$LD"
$B info "$T" | grep -o '"num":3,"first_lba":[0-9]*,"last_lba":[0-9]*'
chkp 3 -

echo
echo "===== 4: 收缩 -30M + rs-after-fs-shrink ====="
mk; M1=$(fillp 1 20)
DISKEDIT_FAULT=rs-after-fs-shrink $BF resize "$T":1 -30M >/dev/null 2>&1
OUT=$($BF resize "$T":1 -30M 2>&1); E=$?
echo "rerun exit=$E"
echo "$OUT" | grep -oE "resuming at chunk [0-9]*|fs shrunk|committed"
$B info "$T" | grep -o '"num":1,"first_lba":[0-9]*,"last_lba":[0-9]*'
chkp 1 -

echo
echo "===== 5: 收缩+搬移（resize-part）rs-chunk:30 ====="
mk; M2=$(fillp 2 20)
echo "-- resize --start 应被显式拒绝（不静默忽略）:"
$BF resize "$T":2 80M --start 900000 2>&1 | head -1
DISKEDIT_FAULT=rs-chunk:30 $BF resize-part "$T":2 --start 900000 --end 1063935 --chunk-size 1 >/dev/null 2>&1
OUT=$($BF resize-part "$T":2 --start 900000 --end 1063935 --chunk-size 1 2>&1); E=$?
echo "rerun exit=$E"
echo "$OUT" | grep -oE "resuming at chunk [0-9]*|fs shrunk|moved|committed"
chkp 2 -

echo
echo "===== 6: grow 路径（尾打包）chunk:150 ====="
mk; M2=$(fillp 2 90); M3=$(fillp 3 190)
DISKEDIT_FAULT=chunk:150 $BF resize "$T":1 grow --allow-move --yes --chunk-size 1 >/dev/null 2>&1
OUT=$($BF resize "$T":1 grow --allow-move --yes --chunk-size 1 2>&1)
R=$(echo "$OUT" | grep -o "resuming at entry [0-9]* chunk [0-9]*" | head -1)
echo "resume: $R (batch 边界应为 chunk 144)"
chkp 2 "$M2"; chkp 3 "$M3"

echo
echo "===== 7: copy 中断（无 ckpt，重跑重做） ====="
T2=/var/tmp/t18b.img; T=$T2
mk; M3=$(fillp 3 190)
$B copy "$T":3 --start 1500000 --chunk-size 1 >/dev/null 2>&1 &
PID=$!; sleep 1; kill -9 $PID 2>/dev/null; wait $PID 2>/dev/null
$B copy "$T":3 --start 1500000 --chunk-size 1 >/dev/null 2>&1; echo "rerun exit=$?"
chkp 3 "$M3"
LD=$(lo_attach "$T")
[ "$M3" = "$(md5sum "${LD}p4" | cut -d' ' -f1)" ] && echo "p4 COPY OK (device-identical)" || { echo "p4 COPY MISMATCH"; rc=1; }
lo_detach "$LD"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }
exit $rc