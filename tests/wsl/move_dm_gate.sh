#!/usr/bin/env bash
# 搬移的 dm 占用闸：dm 栈压着的分区不可搬移（搬移会改变起点，栈里的映射随之指向
# 别处）。LUKS / LVM PV 的类型 GUID 不在搬移逻辑里单独拦，靠这道闸统一判定。
# 判定必须在任何写盘之前（退 3、镜像哈希不变）；dmsetup 缺失则跳过
source "$(dirname "$0")/lib.sh"

if ! command -v dmsetup >/dev/null 2>&1; then
  echo "SKIP: dmsetup not available — 无法在 WSL 里构造 dm 持有者"
  exit 0
fi

require_bin

cleanup_hook() {
  dmsetup remove hold 2>/dev/null
  lo_detach "$LD" 2>/dev/null
}

img T t36.img
truncate -s 512M "$T"
$B new "$T" --yes >/dev/null
# LUKS 类型的分区：类型 GUID 只用于识别，搬移判定依靠占用闸
$B add --yes "$T" 64M --start 2048 --name luks0 --type-guid CA7D7CCB-63ED-4C53-861C-1742536059CC >/dev/null
exp $? 0 "add luks0"

LD=$(lo_attach "$T")
lo_waitpart "${LD}p1" || { echo "loop partition node missing"; exit 1; }
printf 'MAGIC0' | dd of="${LD}p1" bs=1 seek=0 conv=notrunc status=none
SHA_BEFORE=$(sha256sum "$T" | cut -d' ' -f1)
SECTORS=$((64 * 1024 * 1024 / 512))

echo "-- dm 栈压着分区：搬移必须拒绝 --"
dmsetup create hold --table "0 $SECTORS linear ${LD}p1 0" || { echo "dmsetup create failed"; exit 1; }
wait_dm() { for _ in {1..50}; do [ -b /dev/mapper/hold ] && return 0; sleep 0.1; done; return 1; }
wait_dm || { echo "dm node missing"; exit 1; }
OUT=$($B move --yes "$T":1 --start 4096 2>&1); exp $? 3 "dm 持有者拦住搬移"
grep -q "held by" <<<"$OUT" && echo "  OK   拒绝理由指向持有者" \
  || { echo "  BAD  拒绝理由不是持有者: $OUT"; rc=1; }
assert_eq "$(sha256sum "$T" | cut -d' ' -f1)" "$SHA_BEFORE" "  镜像内容未变"

echo "-- 释放 dm 栈后同一搬移成功 --"
dmsetup remove hold && echo "dmsetup remove OK"
$B move --yes "$T":1 --start 4096 >/dev/null 2>&1; exp $? 0 "释放后搬移"
assert_eq "$(dd if="$T" bs=1 skip=$((4096 * 512)) count=6 status=none)" "MAGIC0" "  数据随条目搬到新起点"
assert_eq "$($B info "$T" | grep -o '"num":1,"first_lba":[0-9]*,"last_lba":[0-9]*')" \
  '"num":1,"first_lba":4096,"last_lba":135167' "  表项起点为 4096"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }
exit $rc
