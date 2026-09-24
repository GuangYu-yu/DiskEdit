#!/usr/bin/env bash
# f2fs / vfat / exfat 扩缩运行时 + 4+ 分区复杂布局搬移
source "$(dirname "$0")/lib.sh"
require_bin

cleanup_hook() { rm -f /var/lib/diskedit/* 2>/dev/null; }

T=/var/tmp/t25.img
track_file "$T"

rm -f "$T" "$T"$SIDECAR_GLOB 2>/dev/null

mk1() { # $1 = fs, $2 = size (默认 400M；FAT32 需 ≥512MB)
  rm -f "$T" "$T"$SIDECAR_GLOB
  truncate -s 1G "$T"
  $B new "$T" --yes >/dev/null
  $B create "$T" --size "${2:-400M}" --name p1 --fs "$1" >/dev/null 2>&1
}

echo "########## A: f2fs 扩容 400M → 900M ##########"
mk1 f2fs
echo "before: $($B info "$T" | grep -o '"num":1,[^}]*}')"
LD=$(lo_attach "$T")
mount_at "${LD}p1" /testmnt 2>/dev/null && { dd if=/dev/urandom of=/testmnt/f.bin bs=1M count=100 2>/dev/null; MF=$(md5sum /testmnt/f.bin | cut -d' ' -f1); SZ1=$(df -h --output=size /testmnt | tail -1); echo "before: $SZ1"; umount /testmnt 2>/dev/null; }
lo_detach "$LD"
OUT=$($B resize "$T":1 900M 2>&1); echo "resize exit=$? : $(echo "$OUT" | tail -1)"
LD=$(lo_attach "$T")
mount_at "${LD}p1" /testmnt 2>/dev/null && {
  echo "after : $(df -h --output=size /testmnt | tail -1)"
  [ "$MF" = "$(md5sum /testmnt/f.bin | cut -d' ' -f1)" ] && echo "F2FS DATA OK" || { echo "F2FS DATA CORRUPT"; rc=1; }
  umount /testmnt 2>/dev/null
}
fsck.f2fs -f "${LD}p1" >/dev/null 2>&1; echo "fsck.f2fs exit=$? (0=clean)"
lo_detach "$LD"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "########## B: vfat 扩容（fatresize 在本环境自身崩溃 → 表扩/FS 未扩/有提示）##########"
mk1 vfat 600M
LD=$(lo_attach "$T")
mount_at "${LD}p1" /testmnt 2>/dev/null && { dd if=/dev/urandom of=/testmnt/v.bin bs=1M count=100 2>/dev/null; MV=$(md5sum /testmnt/v.bin | cut -d' ' -f1); echo "before: $(df -h --output=size /testmnt | tail -1)"; umount /testmnt 2>/dev/null; }
lo_detach "$LD"
OUT=$($B resize "$T":1 900M 2>&1); E=$?
echo "resize exit=$E : $(echo "$OUT" | tail -1)"
[ "$E" = "20" ] && echo "B PARTIAL OK（契约：表已改、FS 未扩）" || { echo "B WRONG EXIT ($E)"; rc=1; }
echo "$OUT" | grep -q "fatresize" && echo "B REMEDY HINT OK" || echo "B NO HINT"
echo "B 分区表: $($B info "$T" | grep -o '"num":1,[^}]*}' | grep -o '"size_bytes":[0-9]*')（期望 943718400）"
LD=$(lo_attach "$T")
mount_at "${LD}p1" /testmnt 2>/dev/null && {
  [ "$MV" = "$(md5sum /testmnt/v.bin | cut -d' ' -f1)" ] && echo "VFAT DATA OK" || { echo "VFAT DATA CORRUPT"; rc=1; }
  umount /testmnt 2>/dev/null
}
fsck.vfat -n "${LD}p1" >/dev/null 2>&1; echo "fsck.vfat exit=$? (0=clean)"
lo_detach "$LD"

echo
echo "########## C: exfat 扩容（代码未接线 → 应明确拒绝）##########"
mk1 exfat
LD=$(lo_attach "$T")
mount_at "${LD}p1" /testmnt 2>/dev/null && { echo "exfat 挂载可写测试"; dd if=/dev/urandom of=/testmnt/e.bin bs=1M count=50 2>/dev/null; ME=$(md5sum /testmnt/e.bin | cut -d' ' -f1); umount /testmnt 2>/dev/null; }
lo_detach "$LD"
OUT=$($B resize "$T":1 900M 2>&1); E=$?
echo "resize exit=$E : $(echo "$OUT" | tail -1)"
[ "$E" = "10" ] && echo "C REFUSED OK（无工具 → 事前拒绝，不改盘）" || { echo "C WRONG EXIT ($E)"; rc=1; }
echo "C 分区表: $($B info "$T" | grep -o '"num":1,[^}]*}' | grep -o '"size_bytes":[0-9]*')（应保持 419430400 — REFUSED 未改盘）"
LD=$(lo_attach "$T")
mount_at "${LD}p1" /testmnt 2>/dev/null && { [ "$ME" = "$(md5sum /testmnt/e.bin | cut -d' ' -f1)" ] && echo "EXFAT DATA OK" || { echo "EXFAT DATA CORRUPT"; rc=1; }; umount /testmnt 2>/dev/null; }
fsck.exfat -n "${LD}p1" >/dev/null 2>&1; echo "fsck.exfat exit=$? (0=clean)"
lo_detach "$LD"

echo
echo "########## D: 4+ 分区复杂布局（ESP + MSR + Linux + recovery）搬移 ##########"
rm -f "$T" "$T"$SIDECAR_GLOB
truncate -s 2G "$T"
$B new "$T" --yes >/dev/null
# ESP 200M / MSR 16M / Linux 300M / recovery(NTFS) 500M
$B add "$T" --start 2048 --end 411647 --name ESP --type C12A7328-F81F-11D2-BA4B-00A0C93EC93B >/dev/null
$B add "$T" --start 411648 --end 444415 --name MSR --type E3C9E316-0B5C-4DB8-817D-F92DF00215AE >/dev/null
$B add "$T" --start 444416 --end 1058815 --name Linux >/dev/null
$B add "$T" --start 1058816 --end 2082815 --name recovery --type DE94BBA4-06D1-4D40-A16A-BFD50179D6AC >/dev/null
LD=$(lo_attach "$T")
mkfs.vfat -F32 -n ESP "${LD}p1" >/dev/null
mkfs.ext4 -q -L Linux "${LD}p3"
mount_at "${LD}p1" /testmnt && { echo "esp-file" > /testmnt/efi.txt; umount /testmnt 2>/dev/null; }
mount_at "${LD}p3" /testmnt && { dd if=/dev/urandom of=/testmnt/lin.bin bs=1M count=200 2>/dev/null; umount /testmnt 2>/dev/null; ML=$(md5sum "${LD}p3" | cut -d' ' -f1); }
lo_detach "$LD"
echo "-- 搬移中间的 Linux 分区到尾部空闲区 --"
$B info "$T" | grep -o '"num":[0-9]*,"first_lba":[0-9]*,"last_lba":[0-9]*'
$B move "$T":3 --start 2300000 --chunk-size 4 >/dev/null 2>&1; echo "move exit=$?"
$B info "$T" | grep -o '"num":[0-9]*,"first_lba":[0-9]*,"last_lba":[0-9]*'
echo "-- 未搬移分区的类型 GUID 应原样（ESP/MSR/recovery）--"
for n in 1 2 4; do
  TY=$($B info "$T" | grep -o "\"num\":$n,[^}]*}" | grep -o '"type":"[^"]*"')
  echo "  part$n $TY"
done
LD=$(lo_attach "$T")
[ "$ML" = "$(md5sum "${LD}p3" | cut -d' ' -f1)" ] && echo "D LINUX DEV OK" || { echo "D LINUX DEV CORRUPT"; rc=1; }
e2fsck -fn "${LD}p3" >/dev/null 2>&1 && echo "D p3 e2fsck clean" || { echo "D p3 e2fsck ISSUES"; rc=1; }
mount_at "${LD}p1" /testmnt && { cat /testmnt/efi.txt; umount /testmnt 2>/dev/null; }
lo_detach "$LD"
sgdisk -v "$T" >/dev/null 2>&1 && echo "sgdisk clean" || { echo "sgdisk ISSUES"; rc=1; }

echo
echo "########## E: squashfs + 尾部 overlay（OpenWrt combined）扩容 ##########"
# 依据：src/fsops.rs:1024-1053 resize_fs_in 的 squashfs 分支：定位尾部 RW overlay
# （src/fsid.rs:236-265 overlay_offset_at：squashfs bytes_used@0x28，按 64KiB 上对齐），
# 识别内层 FS（仅 ext/f2fs 可扩，src/fsops.rs:1037-1041），复用既有扩容分发。
# 本用例经 resizefs（离线）触达该分支；resize 的 preflight check_grow 未列 squashfs/erofs
# （src/fsops.rs:812-825 grow_support 的兜底分支），会先拒——该差异见报告，不在此断言。
SQSRC=/var/tmp/t25sqsrc
SQIMG=/var/tmp/t25sq.sqfs
rm -rf "$SQSRC" "$SQIMG"; mkdir -p "$SQSRC"
head -c 2000000 /dev/urandom > "$SQSRC/blob.bin" 2>/dev/null
mksquashfs "$SQSRC" "$SQIMG" -noappend >/dev/null 2>&1; echo "mksquashfs exit=$?"
BU=$(od -An -tu8 -j 40 -N8 "$SQIMG" | tr -d ' ')
OFF=$(( (BU + 65535) / 65536 * 65536 ))
echo "squashfs bytes_used=$BU overlay_off=$OFF"
rm -f "$T" "$T"$SIDECAR_GLOB
truncate -s 512M "$T"
$B new "$T" --yes >/dev/null
$B add "$T" --start 2048 --end 524287 --name rootfs >/dev/null
PS=$((2048 * 512)); PLEN=$(( (524287 - 2048 + 1) * 512 ))
dd if="$SQIMG" of="$T" bs=512 seek=2048 conv=notrunc 2>/dev/null
INNER=$(( (PLEN - OFF) / 2 ))
LO1=$(losetup -f --show -o $((PS + OFF)) --sizelimit "$INNER" "$T")
mkfs.ext4 -q -F "$LO1" >/dev/null 2>&1; echo "mkfs inner ext4 exit=$?"
B0=$(dumpe2fs -h "$LO1" 2>/dev/null | awk '/^Block count:/ {print $3}')
losetup -d "$LO1"
OUT=$($B resizefs "$T":1 2>&1); E=$?
echo "resizefs exit=$E : $(echo "$OUT" | tail -1)"
LO2=$(losetup -f --show -o $((PS + OFF)) --sizelimit "$((PLEN - OFF))" "$T")
B1=$(dumpe2fs -h "$LO2" 2>/dev/null | awk '/^Block count:/ {print $3}')
losetup -d "$LO2"
echo "inner blocks: $B0 -> $B1"
if [ "$E" = "0" ] && [ -n "$B0" ] && [ -n "$B1" ] && [ "$B1" -gt "$B0" ]; then
  echo "E SQUASHFS-OVERLAY-GROW OK"
else
  echo "E NO GROW (exit=$E, $B0 -> $B1)"; rc=1
fi
rm -rf "$SQSRC" "$SQIMG" 2>/dev/null

echo
echo "########## F: erofs + 尾部 overlay 扩容 ##########"
if ! command -v mkfs.erofs >/dev/null 2>&1; then
  echo "SKIP: erofs-utils not installed (mkfs.erofs missing) — 按约定不在 WSL 安装依赖，CI 上真测"
else
# 与 E 同构，覆盖 overlay_offset_at 的 erofs 分支（src/fsid.rs:246-257）：
# blkszbits@1024+0x0C、blocks(u32)@1024+0x24，overlay 偏移 = blocks << blkszbits，
# 64KiB 上对齐后定位内层 RW overlay（mkfs.erofs / erofs-utils，CI 已装）
ERSRC=/var/tmp/t25ersrc
ERIMG=/var/tmp/t25er.img
rm -rf "$ERSRC" "$ERIMG"; mkdir -p "$ERSRC"
head -c 2000000 /dev/urandom > "$ERSRC/blob.bin" 2>/dev/null
mkfs.erofs "$ERIMG" "$ERSRC" >/dev/null 2>&1; echo "mkfs.erofs exit=$?"
BLKSZ=$(od -An -tu1 -j 1036 -N1 "$ERIMG" | tr -d ' ')
# blocks 为超级块内偏移 0x24 的 u32：文件内偏移 = 1024+36 = 1060（0x10 处是 inos，勿混）
NBLK=$(od -An -tu4 -j 1060 -N4 "$ERIMG" | tr -d ' ')
OFF=$(( ((NBLK << BLKSZ) + 65535) / 65536 * 65536 ))
echo "erofs blkszbits=$BLKSZ blocks=$NBLK overlay_off=$OFF"
rm -f "$T" "$T"$SIDECAR_GLOB
truncate -s 512M "$T"
$B new "$T" --yes >/dev/null
$B add "$T" --start 2048 --end 524287 --name rootfs >/dev/null
PS=$((2048 * 512)); PLEN=$(( (524287 - 2048 + 1) * 512 ))
dd if="$ERIMG" of="$T" bs=512 seek=2048 conv=notrunc 2>/dev/null
INNER=$(( (PLEN - OFF) / 2 ))
LO1=$(losetup -f --show -o $((PS + OFF)) --sizelimit "$INNER" "$T")
mkfs.ext4 -q -F "$LO1" >/dev/null 2>&1; echo "mkfs inner ext4 exit=$?"
B0=$(dumpe2fs -h "$LO1" 2>/dev/null | awk '/^Block count:/ {print $3}')
losetup -d "$LO1"
OUT=$($B resizefs "$T":1 2>&1); E=$?
echo "resizefs exit=$E : $(echo "$OUT" | tail -1)"
LO2=$(losetup -f --show -o $((PS + OFF)) --sizelimit "$((PLEN - OFF))" "$T")
B1=$(dumpe2fs -h "$LO2" 2>/dev/null | awk '/^Block count:/ {print $3}')
losetup -d "$LO2"
echo "inner blocks: $B0 -> $B1"
if [ "$E" = "0" ] && [ -n "$B0" ] && [ -n "$B1" ] && [ "$B1" -gt "$B0" ]; then
  echo "F EROFS-OVERLAY-GROW OK"
else
  echo "F NO GROW (exit=$E, $B0 -> $B1)"; rc=1
fi
rm -rf "$ERSRC" "$ERIMG" 2>/dev/null
fi

echo
echo "########## 残留核对（cleanup 前）##########"
echo "t25 files: $(ls /var/tmp/t25* 2>/dev/null | wc -l)  loops: $(losetup -a | wc -l)"
exit $rc