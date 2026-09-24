#!/usr/bin/env bash
# ckpt batching 恢复契约（test-faults 注入版）：
# 恢复只认 durable checkpoint；volatile 进度不得被当作恢复起点
#
# 稳定性：本脚本在同一镜像名上反复重建/重跑，且断言依赖 loop 设备与后备文件两侧视图一致。
# 每次取值前 sync（断言晚于落盘），配合 lib.sh 在 source 时复位 /testmnt，消除跨运行污染。
source "$(dirname "$0")/lib.sh"
require_fault_bin

T=/var/tmp/diskedit_test15.img

track_file "$T"

# p2 400M / chunk 默认 4M → 每条目 100 chunk；batch=16

run_case() {
  local fault="$1" expect="$2" forbid="$3" label="$4"
  rm -f "$T" "$T".diskedit.*
  truncate -s 1G "$T"
  $BF new "$T" --yes >/dev/null
  $BF create "$T" --size 500M --name a >/dev/null
  $BF create "$T" --size 400M --name b --fs ext4 >/dev/null
  LD=$(lo_attach "$T")
  mount_at "${LD}p2" /testmnt
  dd if=/dev/urandom of=/testmnt/blob.bin bs=1M count=350 2>/dev/null
  umount /testmnt
  # 基准哈希必须建立在"已落盘"状态上：dd 经 loop 写入，工具执行时直接读后备文件，
  # 先 sync 让两侧视图对齐，避免把未回写的现场当基准（断言时机早于落盘）
  sync
  MD=$(md5sum "${LD}p2" | cut -d' ' -f1)
  lo_detach "$LD"

  echo "== case: $label =="
  if [ -n "$fault" ]; then
    DISKEDIT_FAULT=$fault $BF resize "$T":1 +100M --allow-move --yes >/dev/null 2>&1
    echo "process aborted (expected)"
  else
    $BF resize "$T":1 +100M --allow-move --yes >/dev/null 2>&1
  fi
  [ -f "$T.diskedit.ckpt" ] && echo "ckpt left: yes" || echo "ckpt left: no"

  # 重跑同命令 → 续传（无注入）
  OUT=$($BF resize "$T":1 +100M --allow-move --yes 2>&1)
  if [ -n "$expect" ]; then
    echo "$OUT" | grep -q "resuming at entry 0 chunk $expect (durable checkpoint)" \
      && echo "RESUME-START OK ($expect)" || { echo "RESUME-START WRONG (want $expect)"; rc=1; echo "$OUT" | grep resuming; }
    if [ -n "$forbid" ]; then
      echo "$OUT" | grep -q "resuming at entry 0 chunk $forbid " \
        && { echo "FORBIDDEN RESUME START ($forbid) APPEARED"; rc=1; } || echo "forbidden start absent (good)"
    fi
  else
    echo "$OUT" | grep -q "resuming" && { echo "UNEXPECTED RESUME"; rc=1; } || echo "clean run (no resume, good)"
  fi

  # 校验哈希同样先 sync：断言只应在数据落盘之后进行，否则会读到旧状态而假报 CORRUPT
  sync
  LD=$(lo_attach "$T")
  MD2=$(md5sum "${LD}p2" | cut -d' ' -f1)
  [ "$MD" = "$MD2" ] && echo "DATA OK" || { echo "DATA CORRUPT"; rc=1; }
  e2fsck -fn "${LD}p2" >/dev/null 2>&1; echo "e2fsck exit=$?"
  lo_detach "$LD"
  sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }
  echo
}

run_case ""           ""    ""   "无注入基线"
run_case "chunk:20"   16    20   "batch 中途崩溃：volatile=20 / durable=16"
run_case "chunk:16"   16    ""   "恰好命中 batch 边界"
run_case "before-entry-commit" 100 "" "末 chunk 后、entry commit 前（final flush 契约）"
exit $rc