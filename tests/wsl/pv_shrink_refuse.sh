#!/usr/bin/env bash
# LVM PV 缩容的拒绝面：已分配 extent 越过新末端时，pvresize 会拒绝、表也不能缩。
# 判据在任何写盘之前给出（退 3），镜像与 PV 都保持原样。
# 依赖 lvm2；WSL 未装则跳过，不安装以免弄脏环境
source "$(dirname "$0")/lib.sh"

if ! command -v pvcreate >/dev/null 2>&1; then
  echo "SKIP: lvm2 not installed (pvcreate missing) — 按约定不在 WSL 安装依赖"
  exit 0
fi

require_bin

cleanup_hook() {
  vgchange -an vgshrink 2>/dev/null
  vgremove -f vgshrink 2>/dev/null
  pvremove -f "${LD}p1" 2>/dev/null
  lo_detach "$LD" 2>/dev/null
}

img T t35.img
rm_img "$T" 2>/dev/null
truncate -s 1G "$T"
$B new "$T" --yes >/dev/null
$B add --yes "$T" 900M --start 2048 --name pv0 --type-guid E6D6D379-F507-44C2-A23C-238F2A3DF928 >/dev/null
exp $? 0 "add pv0"

LD=$(lo_attach "$T")
lo_waitpart "${LD}p1" || { echo "loop partition node missing"; exit 1; }
pvcreate "${LD}p1" >/dev/null && echo "pvcreate OK"
vgcreate vgshrink "${LD}p1" >/dev/null && echo "vgcreate OK"
# 800M 的 LV 占满前 200 个 extent，末端约 839MB，越过 500M 的新末端
lvcreate -L 800M -n lv1 vgshrink >/dev/null && echo "lvcreate OK"
vgchange -an vgshrink >/dev/null && echo "vgchange -an OK"
PV_BEFORE=$(pvs --devices "${LD}p1" --noheadings --nosuffix --units b -o pv_size "${LD}p1" | tr -d ' ')
P1_BEFORE=$($B info "$T" | grep -o '"num":1,"first_lba":[0-9]*,"last_lba":[0-9]*')
SHA_BEFORE=$(sha256sum "$T" | cut -d' ' -f1)

echo "-- 缩到 500M：已分配 extent 越界，写盘前拒绝 --"
OUT=$($B resize --yes "$T":1 500M 2>&1); exp $? 3 "越界缩容拒绝"
grep -q "allocated extents" <<<"$OUT" && echo "  OK   拒绝理由指向已分配 extent" \
  || { echo "  BAD  拒绝理由不是已分配 extent: $OUT"; rc=1; }
assert_eq "$(sha256sum "$T" | cut -d' ' -f1)" "$SHA_BEFORE" "  镜像内容未变"
assert_eq "$($B info "$T" | grep -o '"num":1,"first_lba":[0-9]*,"last_lba":[0-9]*')" "$P1_BEFORE" "  分区表未变"
assert_eq "$(pvs --devices "${LD}p1" --noheadings --nosuffix --units b -o pv_size "${LD}p1" | tr -d ' ')" "$PV_BEFORE" "  PV 元数据未变"
exit $rc
