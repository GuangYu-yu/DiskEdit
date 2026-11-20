#!/usr/bin/env bash
# 全量回归：必须以 root 运行（losetup/mount 需要内核权限）
# 判定规则：脚本进程退出码非 0 即失败；OK/CORRUPT 等关键字行仅作为现场输出展示
# DISKEDIT_STRICT=1（CI 用）时 SKIP 直接判失败——CI 依赖齐全，跳过即缺陷
# 失败留档不写仓库：落在 /var/tmp 下的固定目录（WSL 之内、跨次保留、可 DISKEDIT_WSL_ARTIFACTS 重定向）
set -u
cd "$(dirname "$0")" || exit 1
WSL_ARTIFACTS=${DISKEDIT_WSL_ARTIFACTS:-/var/tmp/diskedit-wsl-artifacts}

SHOW_RE='OK|WRONG|CORRUPT|ISSUES|clean|weak|RESUME|BOUND|resuming|committed|exit=|MOUNT-FAIL|REFUSED|BOOT|JOURNAL|RECORD-STREAMING|refused|marker|MISMATCH|BAD|CHANGED|APPEARED|BROKEN|DIFFERS|WIPED|FAILED|LOST|ODD|NOT REFUSED|INCONSISTENT|unexpected|not reported|SKIP'
pass=0; fail=0; skip=0; failed=(); skipped=()

run() {
  local f=$1 out rc t=900 full=0 declared
  # 单个脚本的运行属性由脚本自己声明，本文件不维护第二份名单：
  #   TIMEOUT=<秒>          覆盖默认的 900 秒
  #   RUNNER_FULL_OUTPUT=1  不过滤输出（每行都是独立失效点）
  declared=$(sed -n 's/^TIMEOUT=\([0-9][0-9]*\)$/\1/p' "$f" | head -n 1)
  [ -n "$declared" ] && t=$declared
  grep -qx 'RUNNER_FULL_OUTPUT=1' "$f" && full=1
  echo "=== $f"
  # -k 30：TERM 后 30 秒仍未退出就 KILL（EXIT trap 不执行，loop 留在现场）
  out=$(timeout -k 30 "$t" bash "$f" 2>&1); rc=$?
  if [ "$full" = 1 ]; then
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

# 脚本集合由目录派生：新增脚本自动纳入，不维护第二份名单。
# 只有「不是用例」的文件才在 SKIP_FILES 里排除，且必须在此处说明原因
SKIP_FILES=(lib.sh run_all.sh)
skip_file() {
  local f
  for f in "${SKIP_FILES[@]}"; do [ "$f" = "$1" ] && return 0; done
  return 1
}

shopt -s nullglob
for f in *.sh; do
  skip_file "$f" && continue
  run "$f"
done
shopt -u nullglob

echo
echo "==== 汇总：$pass 通过，$skip 跳过，$fail 失败 ===="
if [ $skip -gt 0 ]; then printf '跳过脚本:\n'; printf '  %s\n' "${skipped[@]}"; fi
if [ $fail -gt 0 ]; then printf '失败脚本:\n'; printf '  %s\n' "${failed[@]}"; fi
if [ "${DISKEDIT_STRICT:-0}" = 1 ] && [ $skip -gt 0 ]; then
  echo "DISKEDIT_STRICT=1：跳过视同失败"
  exit 1
fi
if [ $fail -gt 0 ]; then exit 1; fi