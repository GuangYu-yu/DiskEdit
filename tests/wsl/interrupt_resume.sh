#!/usr/bin/env bash
# 中断后恢复：断点由 test-faults 的 DISKEDIT_FAULT 确定性注入（命中即 abort），
# 不用 sleep+kill 赌时间——赌输了断点没命中，脚本会把"根本没中断"当成功假绿
source "$(dirname "$0")/lib.sh"
require_fault_bin

T=/var/tmp/diskedit_test13.img
T2=/var/tmp/diskedit_test13b.img

cleanup_hook() { rm -f /var/tmp/diskedit_test13*.img* 2>/dev/null; }

rm -f /var/tmp/diskedit_test13*.img* 2>/dev/null
track_file "$T"
track_file "$T2"

# resize-part 右移 50M（chunk 1MiB，共 900 块）：在第 N 块落盘后中断，
# 重跑必须从 N 续传、数据逐字节一致
run_resize_part_break() {
  local n=$1 md_before md_after OUT LD
  rm -f "$T" "$T"$SIDECAR_GLOB
  truncate -s 2G "$T"
  $BF new "$T" --yes >/dev/null || { echo "new failed"; rc=1; return; }
  $BF create "$T" --size 900M --name big --fs ext4 >/dev/null || { echo "create failed"; rc=1; return; }

  LD=$(lo_attach "$T")
  lo_waitpart "${LD}p1" || { echo "p1 part node not ready"; rc=1; lo_detach "$LD"; return; }
  mount_at "${LD}p1" /testmnt
  dd if=/dev/urandom of=/testmnt/blob.bin bs=1M count=800 2>/dev/null
  umount /testmnt
  md_before=$(md5sum "${LD}p1" | cut -d' ' -f1)
  lo_detach "$LD"

  echo "== rs-chunk:$n：900M 分区右移 50M（chunk 1MiB），第 $n 块后中断 =="
  DISKEDIT_FAULT=rs-chunk:$n $BF resize-part "$T":1 --start 104448 --end 1947647 --chunk-size 1 >/dev/null 2>&1
  exp "$([ -f "$T$CKPT_SUFFIX" ] && echo yes || echo no)" yes "rs-chunk:$n 现场已建（checkpoint 存在）"
  [ -f "$T$CKPT_SUFFIX" ] || return

  OUT=$($BF resize-part "$T":1 --start 104448 --end 1947647 --chunk-size 1 2>&1); E=$?
  exp "$E" 0 "rs-chunk:$n 重跑完成"
  echo "$OUT" | grep -o "resuming at chunk [0-9]*" | head -1 | grep -qx "resuming at chunk $n" \
    && echo "  OK   续传点命中（resuming at chunk $n）" || { echo "  BAD  未从 chunk $n 续传"; rc=1; }

  LD=$(lo_attach "$T")
  lo_waitpart "${LD}p1" || { echo "p1 part node not ready"; rc=1; lo_detach "$LD"; return; }
  md_after=$(md5sum "${LD}p1" | cut -d' ' -f1)
  assert_eq "$md_after" "$md_before" "rs-chunk:$n 数据逐字节一致"
  e2fsck -fn "${LD}p1" >/dev/null 2>&1 && echo "  OK   e2fsck clean" || { echo "  BAD  e2fsck ISSUES"; rc=1; }
  lo_detach "$LD"
}

# resize grow（a 扩 100M、b 后移打包，chunk 1MiB，共 400 块）：在第 N 块落盘后中断，
# 重跑必须续传且 b 数据逐字节一致
run_shift_break() {
  local n=$1 md_before md_after OUT LD R
  rm -f "$T2" "$T2"$SIDECAR_GLOB
  truncate -s 1G "$T2"
  $BF new "$T2" --yes >/dev/null || { echo "new failed"; rc=1; return; }
  $BF create "$T2" --size 500M --name a >/dev/null || { echo "create a failed"; rc=1; return; }
  $BF create "$T2" --size 400M --name b --fs ext4 >/dev/null || { echo "create b failed"; rc=1; return; }

  LD=$(lo_attach "$T2")
  lo_waitpart "${LD}p2" || { echo "p2 part node not ready"; rc=1; lo_detach "$LD"; return; }
  mount_at "${LD}p2" /testmnt
  dd if=/dev/urandom of=/testmnt/blob2.bin bs=1M count=350 2>/dev/null
  umount /testmnt
  md_before=$(md5sum "${LD}p2" | cut -d' ' -f1)
  lo_detach "$LD"

  echo "== chunk:$n：a 扩 100M、b 后移（chunk 1MiB），第 $n 块后中断 =="
  DISKEDIT_FAULT=chunk:$n $BF resize "$T2":1 +100M --allow-move --yes --chunk-size 1 --no-fs >/dev/null 2>&1
  exp "$([ -f "$T2$CKPT_SUFFIX" ] && echo yes || echo no)" yes "chunk:$n 现场已建（checkpoint 存在）"
  [ -f "$T2$CKPT_SUFFIX" ] || return

  OUT=$($BF resize "$T2":1 +100M --allow-move --yes --chunk-size 1 --no-fs 2>&1); E=$?
  exp "$E" 0 "chunk:$n 重跑完成"
  R=$(echo "$OUT" | grep -o "resuming at entry [0-9]* chunk [0-9]*" | head -1)
  case "$R" in
    "resuming at entry 0 chunk "*) echo "  OK   续传点命中（$R）" ;;
    *) echo "  BAD  未续传（got: ${R:-none}）"; rc=1 ;;
  esac

  LD=$(lo_attach "$T2")
  lo_waitpart "${LD}p2" || { echo "p2 part node not ready"; rc=1; lo_detach "$LD"; return; }
  md_after=$(md5sum "${LD}p2" | cut -d' ' -f1)
  assert_eq "$md_after" "$md_before" "chunk:$n 数据逐字节一致"
  e2fsck -fn "${LD}p2" >/dev/null 2>&1 && echo "  OK   e2fsck clean" || { echo "  BAD  e2fsck ISSUES"; rc=1; }
  lo_detach "$LD"
}

for n in 1 450 899; do run_resize_part_break "$n"; echo; done
for n in 1 200 399; do run_shift_break "$n"; echo; done

sgdisk -v "$T" | tail -2
sgdisk -v "$T2" | tail -2
exit $rc
