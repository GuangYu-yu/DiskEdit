#!/usr/bin/env bash
# 中断后恢复：断点由 test-faults 的 DISKEDIT_FAULT 确定性注入（命中即 abort），
# 不用 sleep+kill 赌时间——赌输了断点没命中，脚本会把"根本没中断"当成功假绿
source "$(dirname "$0")/lib.sh"
require_fault_bin

img T2 resume13b.img

run_shift_break() {
  local n=$1 fault=${2:-chunk} expect=${3:-$1} md_before md_after OUT LD R
  rm -rf "$T2" "$T2"$SIDECAR_GLOB
  truncate -s 1G "$T2"
  $BF new "$T2" --yes >/dev/null || { echo "new failed"; rc=1; return; }
  $BF add "$T2" 500M --align mib --name a --yes >/dev/null || { echo "add a failed"; rc=1; return; }
  $BF add "$T2" 400M --align mib --name b --yes >/dev/null || { echo "add b failed"; rc=1; return; }
  $BF mkfs "$T2":2 ext4 --yes >/dev/null || { echo "mkfs b failed"; rc=1; return; }

  LD=$(lo_attach "$T2")
  lo_waitpart "${LD}p2" || { echo "p2 part node not ready"; rc=1; lo_detach "$LD"; return; }
  mount_at "${LD}p2" "$TEST_MNT"
  dd if=/dev/urandom of="$TEST_MNT/blob2.bin" bs=1M count=350 2>/dev/null
  umount "$TEST_MNT"
  md_before=$(mdsum "${LD}p2")
  lo_detach "$LD"

  echo "== $fault:$n：a 扩 100M、b 后移（chunk 1MiB），第 $n 块后中断 =="
  DISKEDIT_FAULT=$fault:$n $BF resize "$T2":1 +100M --allow-move --yes --chunk 1 --no-fs >/dev/null 2>&1
  exp "$(record_present "$T2" && echo yes || echo no)" yes "$fault:$n 现场已建（记录存在）"
  record_present "$T2" || return
  assert_eq "$(record_cursor "$T2")" "$expect" "$fault:$n 记录游标停在 $expect"

  OUT=$($BF resize "$T2":1 +100M --allow-move --yes --chunk 1 --no-fs 2>&1); E=$?
  exp "$E" 0 "$fault:$n 重跑完成"
  R=$(echo "$OUT" | grep -o "resuming at entry [0-9]* chunk [0-9]*" | head -1)
  [ "$R" = "resuming at entry 0 chunk $expect" ] \
    && echo "  OK   续传点命中（$R）" || { echo "  BAD  未从 entry 0 chunk $expect 续传（got: ${R:-none}）"; rc=1; }

  LD=$(lo_attach "$T2")
  lo_waitpart "${LD}p2" || { echo "p2 part node not ready"; rc=1; lo_detach "$LD"; return; }
  md_after=$(mdsum "${LD}p2")
  assert_eq "$md_after" "$md_before" "chunk:$n 数据逐字节一致"
  e2fsck -fn "${LD}p2" >/dev/null 2>&1 && echo "  OK   e2fsck clean" || { echo "  BAD  e2fsck ISSUES"; rc=1; }
  lo_detach "$LD"
  record_absent "$T2" && echo "  OK   完成后无记录残留" || { echo "  BAD  完成后记录仍在"; rc=1; }
}

for n in 1 200 399; do run_shift_break "$n"; echo; done
run_shift_break 200 after-data-sync 199; echo

SG_OUT=$(sgdisk -v "$T2" 2>&1); SG_RC=$?
printf '%s\n' "$SG_OUT" | tail -2
[ "$SG_RC" -eq 0 ] || { echo "  BAD  sgdisk -v $T2 失败 (exit=$SG_RC)"; rc=1; }
exit $rc
