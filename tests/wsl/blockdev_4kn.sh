#!/usr/bin/env bash
# 4Kn（4096 逻辑扇区）—— 块设备路径
source "$(dirname "$0")/lib.sh"
require_fault_bin

img T t24.img
LD=""

cleanup_hook() {
  [ -n "$LD" ] && lo_detach "$LD" 2>/dev/null
    rm -rf /var/lib/diskedit/* 2>/dev/null
}

rm -rf /var/lib/diskedit/* 2>/dev/null

refresh() { sync; blockdev --rereadpt "$LD" 2>/dev/null; partx -u "$LD" 2>/dev/null; sleep 1; }

# 失败时打印完整输出与退出码：截断到末行会让真正的拒绝原因从日志里消失
fail_report() { # $1=标签 $2=退出码 $3=完整输出
  echo "  BAD  $1 FAILED (exit $2)"
  [ -n "$3" ] && printf '%s\n' "$3" | sed 's/^/    /'
  rc=1
}

# 表已提交、后置条件未满足时的退出码（Partial，见 src/outcome.rs）。
# 脚本在这个码上做内核视图刷新与现场释放，成败只看盘上内容，不解析人类可读文案
RC_PARTIAL=5

# 分区 2 的记录里是否出现给定片段。info 读的是盘上的表，不依赖内核分区视图
part2_has() {
  $B info "$LD" 2>/dev/null | grep -o '"num":2,[^}]*}' | grep -Fq -- "$1"
}

# 释放现场并放弃记录里的表区原文：这些调用点的表项已经写定，recover 没有可回滚的
# 目标。带 --discard-replay 是必须的——记录里留着的表区 preimage 会让不带它的
# recover --release 以 3 拒绝。失败不静默：记录留着会挡住后续每一条表写命令
release_now() {
  local out e
  out=$($B recover --release "$LD" --yes --discard-replay 2>&1); e=$?
  [ "$e" -eq 0 ] || fail_report "recover --release" "$e" "$out"
  return "$e"
}

# 表写命令的稳定执行：DiskEdit 在表提交后校验内核分区视图，reread 竞态会以
# Partial 收场（表已写、记录保留，可 recover）。这一种结果的契约落在盘上：
# 按 $2 核对分区 2 的记录，对上了才刷新内核视图、释放现场并判成功；
# 对不上说明表项没写成该有的样子，按失败原样上报
set4kn() { # $1=label $2=成功后分区 2 记录里应出现的片段  其余=传给 set 的参数
  local label=$1 want=$2 out e; shift 2
  out=$($B set --yes "$LD":2 "$@" 2>&1); e=$?
  if [ "$e" -eq "$RC_PARTIAL" ] && part2_has "$want"; then
    e=0
    echo "  set $label：表已写盘，内核视图滞后"
    record_present "$LD" && release_now
    refresh
  fi
  [ "$e" -eq 0 ] && echo "  OK   set $label" || fail_report "set $label" "$e" "$out"
}

addk() { # $1=label $2=期望生成的分区节点  其余=传给 add 的参数
  local label=$1 node=$2 out e; shift 2
  out=$($B add --yes "$LD" "$@" 2>&1); e=$?
  if [ "$e" -eq "$RC_PARTIAL" ]; then
    partx -u "$LD" 2>/dev/null; sleep 1
    if [ -b "$node" ]; then
      # 表已写、内核视图滞后，DiskEdit 保留了记录——不清掉会以 "unfinished
      # operation" 闸拒绝后续所有表写命令
      if release_now; then
        echo "  OK   add $label（首次已写盘，内核视图滞后，记录已释放）"; return 0
      fi
      return 1
    fi
    record_present "$LD" && $B recover "$LD" --yes >/dev/null 2>&1
    out=$($B add --yes "$LD" "$@" 2>&1); e=$?
  fi
  [ "$e" -eq 0 ] && echo "  OK   add $label" || { fail_report "add $label" "$e" "$out"; return 1; }
}

movek() { # $1=目标 start（LBA）
  local start=$1 out e fl
  out=$($B move "$LD":2 --start "$start" --chunk 4 --yes 2>&1); e=$?
  if [ "$e" -eq "$RC_PARTIAL" ]; then
    partx -u "$LD" 2>/dev/null; sleep 1
    fl=$($B info "$LD" | grep -o '"num":2,"first_lba":[0-9]*' | grep -o '[0-9]*$')
    if [ "$fl" = "$start" ]; then
      e=0
      release_now && echo "  move 首次已写盘，内核视图滞后，记录已释放"
    else
      release_now || return 1
      out=$($B move "$LD":2 --start "$start" --chunk 4 --yes 2>&1); e=$?
    fi
  fi
  echo "$out"; return $e
}

setup4k() {
  local out e
  rm_img "$T"; rm -rf /var/lib/diskedit/* 2>/dev/null
  [ -n "$LD" ] && lo_detach "$LD" 2>/dev/null
  truncate -s 2G "$T"
  LD=$(lo_attach "$T" --sector-size 4096)
  out=$($B new "$LD" --yes 2>&1); e=$?
  if [ "$e" -eq "$RC_PARTIAL" ] && $B info "$LD" 2>/dev/null | grep -q '"gpt","damaged":false'; then
    e=0
    echo "  new：表已写盘，内核视图滞后"
    record_present "$LD" && release_now
    refresh
  fi
  [ "$e" -eq 0 ] || { echo "setup: new FAILED (exit $e)"; printf '%s\n' "$out" | sed 's/^/    /'; rc=1; return 1; }
  # 4Kn 下 1MiB = 256 扇区
  addk p1 "${LD}p1" 268435456 --start 256 --name p1 || return 1
  refresh; mkfs.ext4 -q "${LD}p1" || { echo "setup: mkfs p1 FAILED"; rc=1; return 1; }
  addk p2 "${LD}p2" 268435456 --start 131072 --name p2 || return 1
  refresh; mkfs.ext4 -q "${LD}p2" || { echo "setup: mkfs p2 FAILED"; rc=1; return 1; }
  addk p3 "${LD}p3" 268435456 --start 262144 --name p3 || return 1
  refresh; mkfs.ext4 -q "${LD}p3" || { echo "setup: mkfs p3 FAILED"; rc=1; return 1; }
}

echo "########## A: 块设备 4Kn 识别 ##########"
setup4k
echo "loop: $LD  logical_ss=$(blockdev --getss "$LD")"
$B info "$LD" | head -c 130; echo
echo "-- 期望 sector_size=4096 且 LBA 与 256 对齐 --"
$B info "$LD" | grep -o '"num":[0-9]*,"first_lba":[0-9]*,"last_lba":[0-9]*'
mount_at "${LD}p2" "$TEST_MNT" && { dd if=/dev/urandom of="$TEST_MNT/blob" bs=1M count=200 2>/dev/null; echo "p2 200M written"; umount "$TEST_MNT"; M2=$(mdsum "${LD}p2"); }

echo
echo "########## B: 4Kn 自重叠左移（delta < 分区长度）##########"
MOVE_OUT=$(movek 66048); MOVE_E=$?
if [ "$MOVE_E" -eq 0 ]; then echo "move exit=0"; else fail_report "move 66048" "$MOVE_E" "$MOVE_OUT"; fi
$B info "$LD" | grep -o '"num":2,[^}]*}'
refresh
e2fsck -fn "${LD}p2" >/dev/null 2>&1; echo "e2fsck -fn exit=$? (0=clean)"
[ "$M2" = "$(mdsum "${LD}p2")" ] && echo "4KN DEV OK" || { echo "4KN DEV CORRUPT"; rc=1; }
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; sgdisk -v "$T" 2>&1 | sed 's/^/    /'; }

echo
echo "########## C: 4Kn 中断恢复（fault 注入）##########"
setup4k
mount_at "${LD}p2" "$TEST_MNT" && { dd if=/dev/urandom of="$TEST_MNT/blob" bs=1M count=200 2>/dev/null; umount "$TEST_MNT"; M2=$(mdsum "${LD}p2"); }
DISKEDIT_FAULT=chunk:20 $BF move "$LD":2 --start 66048 --chunk 4 --yes >/dev/null 2>&1
echo "现场：$(ls /var/lib/diskedit/ 2>/dev/null | wc -l) 项"
OUT=$($B move "$LD":2 --start 66048 --chunk 4 --yes 2>&1)
R=$(echo "$OUT" | grep -o 'resuming at chunk [0-9]*')
[ "$R" = "resuming at chunk 20" ] && echo "resume: $R" || { echo "resume: WRONG ($R)"; printf '%s\n' "$OUT" | sed 's/^/    /'; rc=1; }
refresh
[ "$M2" = "$(mdsum "${LD}p2")" ] && echo "4KN RESUME DEV OK" || { echo "4KN RESUME DEV CORRUPT"; rc=1; }
e2fsck -fn "${LD}p2" >/dev/null 2>&1 && echo "4KN RESUME e2fsck clean" || { echo "4KN RESUME e2fsck ISSUES"; rc=1; }

echo
echo "########## D: 分区身份/属性搬移后保持（4Kn 块设备）##########"
setup4k
set4kn "name keepname" '"name":"keepname"' name keepname
set4kn "flag esp on" '"type":"C12A7328-F81F-11D2-BA4B-00A0C93EC93B"' flag esp on
echo "move 前现场：$(ls /var/lib/diskedit/ 2>/dev/null | wc -l) 项"
PU_BEFORE=$(sgdisk -i 2 "$LD" 2>/dev/null | grep -i "unique GUID" | awk '{print $NF}')
TY_BEFORE=$($B info "$LD" | grep -o '"num":2,[^}]*}' | grep -o '"type":"[^"]*"')
assert_eq "$TY_BEFORE" '"type":"C12A7328-F81F-11D2-BA4B-00A0C93EC93B"' "D 搬移前类型为 esp"
MOVE_OUT=$(movek 66048); MOVE_E=$?
if [ "$MOVE_E" -eq 0 ]; then echo "move exit=0"; else fail_report "move 66048" "$MOVE_E" "$MOVE_OUT"; fi
PU_AFTER=$(sgdisk -i 2 "$LD" 2>/dev/null | grep -i "unique GUID" | awk '{print $NF}')
TY_AFTER=$($B info "$LD" | grep -o '"num":2,[^}]*}' | grep -o '"type":"[^"]*"')
NAME_AFTER=$($B info "$LD" | grep -o '"num":2,[^}]*}' | grep -o '"name":"[^"]*"')
[ "$PU_BEFORE" = "$PU_AFTER" ] && [ -n "$PU_BEFORE" ] && echo "D PARTUUID PRESERVED OK" || { echo "D PARTUUID CHANGED ($PU_BEFORE -> $PU_AFTER)"; rc=1; }
[ "$TY_BEFORE" = "$TY_AFTER" ] && echo "D TYPE-GUID(esp) PRESERVED OK" || { echo "D TYPE-GUID CHANGED"; rc=1; }
echo "$NAME_AFTER" | grep -q keepname && echo "D NAME PRESERVED OK" || { echo "D NAME LOST"; rc=1; }

echo
echo "########## 残留核对（cleanup 前）##########"
echo "root files: $(ls "$WSL_ROOT" 2>/dev/null | wc -l)  loops: $(losetup -a | wc -l)"
exit $rc