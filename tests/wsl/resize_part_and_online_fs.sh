#!/usr/bin/env bash
# 分区扩容 + 挂载态在线扩 FS：resize-part 改表后由 resizefs --online 把 FS 填满
source "$(dirname "$0")/lib.sh"
require_bin

T=/var/tmp/t31.img
MP=/testmnt
track_file "$T"

fs_bytes() { # $1=设备：Block count × Block size
  dumpe2fs -h "$1" 2>/dev/null | awk '/^Block count:/ {c=$3} /^Block size:/ {s=$3} END {print c*s}'
}

rm -f "$T" "$T"$SIDECAR_GLOB 2>/dev/null
truncate -s 64M "$T"

echo "== new + create p1（16MiB ext4）=="
$B new "$T" --yes >/dev/null; exp $? 0 "new"
$B create "$T" --size 16M --name p1 --fs ext4 >/dev/null; exp $? 0 "create p1（16MiB ext4）"

echo "== resize-part 扩到 67583（=32MiB）=="
$B resize-part "$T":1 --start 2048 --end 67583 >/dev/null; exp $? 0 "resize-part 扩到 67583"
$B info "$T" | grep -q '"num":1,[^}]*"last_lba":67583' \
  && echo "  OK   分区已到 2048..67583" || { echo "  BAD  分区未到 67583"; rc=1; }

# loop 必须在改表之后建立：内核分区视图在 losetup 时定型，先建会拿到旧尺寸
LD=$(lo_attach "$T")

echo "== 挂载 + 写 marker =="
mount_at "${LD}p1" "$MP" || { echo "  BAD  mount 失败"; rc=1; exit 1; }
echo marker-online > "$MP/marker"
PART_BYTES=$(blockdev --getsize64 "${LD}p1")

echo "== resizefs --online =="
$B resizefs "$MP" --online >/dev/null; exp $? 0 "resizefs --online"
[ "$(fs_bytes "${LD}p1")" = "$PART_BYTES" ] && echo "  OK   FS 已填满分区" \
  || { echo "  BAD  FS=$(fs_bytes "${LD}p1") 分区=$PART_BYTES"; rc=1; }
[ "$(cat "$MP/marker" 2>/dev/null)" = "marker-online" ] && echo "  OK   marker 存活" \
  || { echo "  BAD  marker 丢失"; rc=1; }

umount "$MP"
$B check "$T":1 >/dev/null; exp $? 0 "check p1"
sgdisk -v "$T" >/dev/null 2>&1 && echo "  OK   sgdisk -v" || { echo "  BAD  sgdisk 报问题"; rc=1; }

echo
echo "==== 在线扩容结束（rc=$rc）===="
exit $rc