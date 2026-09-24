#!/usr/bin/env bash
# 全量回归：必须以 root 运行（losetup/mount 需要内核权限）
# 判定规则：脚本进程退出码非 0 即失败；OK/CORRUPT 等关键字行仅作为现场输出展示，
# 不参与判定（历史版本用 grep 结果当判定，脚本失败会被管道吞掉）
set -u
cd "$(dirname "$0")" || exit 1

SHOW_RE='OK|WRONG|CORRUPT|ISSUES|clean|weak|RESUME|BOUND|resuming|committed|exit=|MOUNT-FAIL|REFUSED|BOOT|JOURNAL|refused|marker|MISMATCH|BAD|CHANGED|APPEARED|BROKEN|DIFFERS|WIPED|FAILED|LOST|ODD|NOT REFUSED|INCONSISTENT|unexpected|not reported'
pass=0; fail=0; failed=()

run() {
  local f=$1 out rc
  echo "=== $f"
  out=$(timeout 900 bash "$f" 2>&1); rc=$?
  grep -E "$SHOW_RE" <<<"$out" | head -60
  if [ $rc -eq 0 ]; then
    echo "--- PASS (rc=0)"; pass=$((pass+1))
  else
    echo "--- FAIL (rc=$rc)"; tail -8 <<<"$out" | sed 's/^/    /'
    fail=$((fail+1)); failed+=("$f")
  fi
}

for f in \
  selftest.sh \
  smoke_full_commands.sh \
  resize_move_resizefs_chain.sh \
  resize_part_and_online_fs.sh \
  online_guards.sh \
  size_units_and_refusals.sh \
  arg_guards.sh \
  middle_partition_undo.sh \
  interrupt_resume.sh \
  shift_no_kill_baseline.sh \
  ckpt_batch_contract.sh \
  ckpt_crash_windows.sh \
  ckpt_replay_idempotency.sh \
  resize_part_fault_paths.sh \
  command_boundary_pairs.sh \
  blockdev_ckpt_edges.sh \
  fs_xfs_btrfs_ntfs.sh \
  usage_misc.sh \
  journal_tree_tornwrite.sh \
  blockdev_4kn.sh \
  fs_f2fs_vfat_exfat_layout.sh \
  postcondition_contract.sh \
  uncovered_branches.sh \
  boundary_last_usable.sh \
  mbr_delete_chain.sh \
  capacity_concurrency_readonly.sh \
  lvm_pv_online.sh \
  lvm_gate.sh \
  qemu_boot_verification.sh
do
  run "$f"
done

echo
echo "==== 汇总：$pass 通过，$fail 失败 ===="
if [ $fail -gt 0 ]; then
  printf '失败脚本:\n'; printf '  %s\n' "${failed[@]}"
  exit 1
fi