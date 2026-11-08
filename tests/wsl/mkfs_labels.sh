#!/usr/bin/env bash
# mkfs --label 逐类型写入 + 官方工具回读（含能力缺口类型）。
#
# 每种可建卷的类型：建分区 → mkfs 时带 --label 写入 → 用该类型的官方工具回读 →
# 断言回读值与写入值逐字节一致；再用 info 断言识别出的规范类型名（ext2/3/4 统一为
# ext，见 src/fsid.rs）。ext 系额外跑 e2fsck -fn，确认建卷产物本身一致。
#
# 负向覆盖「不接受这个卷标的类型」：
#   - 定长字段放不下（vfat 11 字节、xfs 12 字节）→ 3，且整镜像 sha256 不变；
#   - vfat 只收 ASCII（字段容量按编码后字节计）→ 3，同样零写入；
#   - 可解析但不能建卷的类型（lvm2_pv）→ 6 与专用文案；
#   - 未登记的类型名（zfs）→ 6 与 supported 列表。
#
# 所需外部工具缺失时直接失败，不打印 SKIP：在 DISKEDIT_STRICT=1 下跳过不算覆盖。
source "$(dirname "$0")/lib.sh"
require_bin

for tool in blkid e2label xfs_admin btrfs fatlabel exfatlabel ntfslabel swaplabel e2fsck; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        echo "缺少回读工具: $tool（安装该类型的官方工具包后重跑）" >&2
        exit 1
    fi
done

img T mkfs-label.img
rm -rf "$T" "$T"$SIDECAR_GLOB

# 1 GiB 载体 + 400 MiB 单分区。--align mib 让起止可预期（mke2fs 的预备扇区与
# 各家的最小尺寸都远低于 400 MiB）
setup() {
    rm -rf "$T" "$T"$SIDECAR_GLOB
    truncate -s 1G "$T" || { echo "  BAD  truncate 失败"; rc=1; return 1; }
    $B new "$T" --yes >/dev/null || { echo "  BAD  夹具: new 失败"; rc=1; return 1; }
    $B add "$T" 400M --align mib --name t --yes >/dev/null || { echo "  BAD  夹具: add 失败"; rc=1; return 1; }
}

# 各类型的官方回读入口（不类推参数：读法同样按各自手册）
read_label() { # fs 分区设备
    case "$1" in
        ext2|ext3|ext4) e2label "$2" ;;
        xfs)            xfs_admin -l "$2" 2>/dev/null | sed -n 's/^label = "\(.*\)"$/\1/p' ;;
        btrfs)          btrfs filesystem label "$2" 2>/dev/null ;;
        f2fs)           blkid -p -s LABEL -o value "$2" 2>/dev/null ;;
        vfat)           fatlabel "$2" 2>/dev/null ;;
        exfat)          exfatlabel "$2" 2>/dev/null ;;
        ntfs)           ntfslabel "$2" 2>/dev/null ;;
        swap)           swaplabel "$2" 2>/dev/null | awk '/^LABEL:/{print $2}' ;;
    esac
}

run_case() { # fs 标签 info 中的类型名
    local fs=$1 label=$2 want=$3 dev got
    echo "===== mkfs $fs --label $label → 回读 → info ====="
    setup || return 1
    $B mkfs "$T":1 "$fs" --label "$label" --yes >/dev/null 2>&1
    exp "$?" 0 "mkfs 建立 $fs 并写入卷标"
    dev=$(lo_attach "$T") || { echo "  BAD  losetup 失败"; rc=1; return 1; }
    if ! lo_waitpart "${dev}p1"; then
        echo "  BAD  ${dev}p1 未就绪"; rc=1; lo_detach "$dev"; return 1
    fi
    got=$(read_label "$fs" "${dev}p1")
    if [ "$fs" = ext2 ] || [ "$fs" = ext3 ] || [ "$fs" = ext4 ]; then
        if e2fsck -fn "${dev}p1" >/dev/null 2>&1; then
            echo "  OK   $fs e2fsck clean"
        else
            echo "  BAD  $fs e2fsck 报错"; rc=1
        fi
    fi
    lo_detach "$dev"
    assert_eq "$got" "$label" "$fs 卷标回读"
    assert_eq "$($B info "$T" | grep -o '"fs":"[^"]*"' | head -1)" "\"fs\":\"$want\"" "$fs 的 info 类型名"
}

run_case ext2  EXT2VOL  ext
run_case ext3  EXT3VOL  ext
run_case ext4  EXT4VOL  ext
run_case xfs   XFSVOL   xfs
run_case btrfs BTRFSVOL btrfs
run_case f2fs  F2FSVOL  f2fs
run_case vfat  FATVOL   vfat
run_case exfat EXFATVOL exfat
run_case ntfs  NTSVOL   ntfs
run_case swap  SWAPVOL  swap

# 负向：拒绝必须发生在擦除第一个字节之前（整镜像 sha256 不变），且不留记录现场
neg() { # 期望退出码 fs 标签 期望文案 描述
    local want=$1 fs=$2 label=$3 msg=$4 what=$5 out e before after
    echo "===== 负向: $what ====="
    setup || return 1
    before=$(sha256sum "$T" | cut -d' ' -f1)
    out=$($B mkfs "$T":1 "$fs" --label "$label" --yes 2>&1); e=$?
    exp "$e" "$want" "$what"
    if [ -n "$msg" ]; then
        if grep -q -- "$msg" <<<"$out"; then
            echo "  OK   文案含: $msg"
        else
            echo "  BAD  文案缺少: $msg"; printf '%s\n' "$out" | sed 's/^/    /'; rc=1
        fi
    fi
    after=$(sha256sum "$T" | cut -d' ' -f1)
    assert_eq "$after" "$before" "$what 零写入（整镜像 sha256 不变）"
    if record_absent "$T"; then
        echo "  OK   无残留记录"
    else
        echo "  BAD  拒绝后留下记录现场"; rc=1
    fi
}

neg 3 vfat "ABCDEFGHIJKL" "exceeds the 11-byte on-disk limit" "vfat 12 字节卷标"
neg 3 vfat "VOL中文" "cannot be represented in a DOS code page" "vfat 非 ASCII 卷标"
neg 3 xfs  "0123456789ABC" "xfs label exceeds 12 bytes" "xfs 13 字节卷标"
neg 6 lvm2_pv "PVX" "cannot mkfs an LVM2 PV" "lvm2_pv 不能建卷"
neg 6 zfs "ZFSX" "unsupported fstype zfs" "未登记类型 zfs"

exit $rc
