#!/usr/bin/env bash
# 全量回归：必须以 root 运行（losetup/mount 需要内核权限）
# 判定规则：脚本进程退出码非 0 即失败；OK/CORRUPT 等关键字行仅作为现场输出展示，不参与判定
# 脚本在输出里打印以 `SKIP:` 开头的行＝有子项因依赖缺失被跳过：记 SKIP，既不算通过
# 也不算失败。DISKEDIT_STRICT=1（CI 用）时 SKIP 直接判失败——CI 依赖齐全，跳过即缺陷
# 失败留档不写仓库：落在 /var/tmp 下的固定目录（WSL 之内、跨次保留、可 DISKEDIT_WSL_ARTIFACTS
# 重定向），脚本自身的临时产物由 lib.sh 的 WSL_ROOT 收口，两条路径互不相干
set -u
cd "$(dirname "$0")" || exit 1
WSL_ARTIFACTS=${DISKEDIT_WSL_ARTIFACTS:-/var/tmp/diskedit-wsl-artifacts}

SHOW_RE='OK|WRONG|CORRUPT|ISSUES|clean|weak|RESUME|BOUND|resuming|committed|exit=|MOUNT-FAIL|REFUSED|BOOT|JOURNAL|RECORD-STREAMING|refused|marker|MISMATCH|BAD|CHANGED|APPEARED|BROKEN|DIFFERS|WIPED|FAILED|LOST|ODD|NOT REFUSED|INCONSISTENT|unexpected|not reported|SKIP'
pass=0; fail=0; skip=0; failed=(); skipped=()

run() {
  local f=$1 out rc t=900
  case "$f" in
    blockdev_probe_identity.sh) t=1800 ;;
  esac
  echo "=== $f"
  # -k 30：TERM 之后 30 秒仍未退出就 KILL，避免单个脚本卡死整套回归。
  # KILL 下脚本的 EXIT trap 不执行，loop/挂载/镜像留在现场，只能下次开跑复位或手动清
  out=$(timeout -k 30 "$t" bash "$f" 2>&1); rc=$?
  # 命令面校验的每一行都是一处失效点：全量展示，不按关键字过滤
  if [ "$f" = check_cli_surface.sh ]; then
    printf '%s\n' "$out"
  else
    grep -E "$SHOW_RE" <<<"$out" | head -60
  fi
  if [ $rc -ne 0 ]; then
    echo "--- FAIL (rc=$rc)"; tail -8 <<<"$out" | sed 's/^/    /'
    # 完整输出留档：过滤行只挑关键字，脚本中途静默退出时看不出是哪条命令
    mkdir -p "$WSL_ARTIFACTS"
    printf '%s\n' "$out" > "$WSL_ARTIFACTS/${f%.sh}.log"
    echo "--- full output: $WSL_ARTIFACTS/${f%.sh}.log"
    fail=$((fail+1)); failed+=("$f")
  elif grep -q '^SKIP:' <<<"$out"; then
    echo "--- SKIP ($(grep -m1 '^SKIP:' <<<"$out"))"
    skip=$((skip+1)); skipped+=("$f")
  else
    echo "--- PASS (rc=0)"; pass=$((pass+1))
  fi
}

for f in \
  check_cli_surface.sh \
  selftest.sh \
  smoke_full_commands.sh \
  resize_move_resizefs_chain.sh \
  resize_part_and_online_fs.sh \
  online_guards.sh \
  online_btrfs_identity.sh \
  size_units_and_refusals.sh \
  arg_guards.sh \
  middle_partition_undo.sh \
  interrupt_resume.sh \
  shift_no_kill_baseline.sh \
  resume_batch_contract.sh \
  resume_crash_windows.sh \
  resume_replay_idempotency.sh \
  resize_part_fault_paths.sh \
  command_boundary_pairs.sh \
  blockdev_resume_edges.sh \
  fs_xfs_btrfs_ntfs.sh \
  usage_misc.sh \
  record_tree_tornwrite.sh \
  blockdev_4kn.sh \
  fs_f2fs_vfat_exfat_layout.sh \
  mkfs_labels.sh \
  postcondition_contract.sh \
  mount_cleanup_pending.sh \
  shrink_readback.sh \
  shrink_recheck_gpt.sh \
  shrink_recheck_mbr.sh \
  uncovered_branches.sh \
  boundary_last_usable.sh \
  mbr_delete_chain.sh \
  capacity_concurrency_readonly.sh \
  carrier_and_image_occupancy.sh \
  image_loop_identity.sh \
  blockdev_probe_identity.sh \
  lvm_pv_online.sh \
  lvm_resume.sh \
  fs_grow_exit.sh \
  check_exit_codes.sh \
  lvm_gate.sh \
  review_guards.sh \
  qemu_boot_verification.sh
do
  run "$f"
done

echo
echo "==== 汇总：$pass 通过，$skip 跳过，$fail 失败 ===="
if [ $skip -gt 0 ]; then printf '跳过脚本:\n'; printf '  %s\n' "${skipped[@]}"; fi
if [ $fail -gt 0 ]; then printf '失败脚本:\n'; printf '  %s\n' "${failed[@]}"; fi
if [ "${DISKEDIT_STRICT:-0}" = 1 ] && [ $skip -gt 0 ]; then
  echo "DISKEDIT_STRICT=1：跳过视同失败"
  exit 1
fi
if [ $fail -gt 0 ]; then exit 1; fi