#!/usr/bin/env bash
# 镜像占用反查的身份判定：以 `LOOP_GET_STATUS64` 的 (lo_device, lo_inode) 与
# 已打开文件句柄的 (st_dev, st_ino) 比对，取代 sysfs `loop/backing_file` 路径匹配。
# 覆盖：
#   A. 编码一致性——同一 backing file 上，ioctl 返回的身份与 fstat 相等
#   B. 路径不可解析——从硬链接建 loop 后删除该链接，占用仍须被拒
#   E. 块设备栈持有——loop 本身或其分区被 dm 映射时改表仍须被拒
#   F. 活动 swap——分区作 swap 时改表仍须被拒
#   C. 跨 mount namespace——loop 与挂载都在另一 namespace 内，本 namespace 仍须检出
#   D. 不可确认即拒绝——/dev 下看不到 loop 节点时退出码 30，且不得写成"空闲"
#   G. 进程退出竞态——已退出进程的 mountinfo 是 EINVAL，跳过它不得掩盖真实探测故障
#   G2. 权限不足——hidepid 下读不到他人 mountinfo 时按探测失败拒绝，不得当成"未挂载"
#   H. 多 loop 归属——同一镜像的两个 loop 中只有一个被持有，拒绝消息须指向它
# 依赖 root 与 util-linux（losetup / mount / unshare）；A 另需 python3。
source "$(dirname "$0")/lib.sh"
require_bin

[ "$(id -u)" -eq 0 ] || { echo "SKIP: 需要 root（losetup / mount / unshare）"; exit 0; }
command -v losetup >/dev/null || { echo "SKIP: losetup 缺失"; exit 0; }
command -v mkfs.ext4 >/dev/null || { echo "SKIP: mkfs.ext4 缺失"; exit 0; }
unshare -m true 2>/dev/null || { echo "SKIP: unshare -m 不可用"; exit 0; }

T=$(mktemp /tmp/loopid-XXXXXX.img)
track_file "$T"
truncate -s 96M "$T"

# C/E/F 段的收尾在失败路径也要执行：先定义，变量在用到时才赋值
DMNAME=
SWAPDEV=
ZPARENT=
NSPID=
NLOOP=
NS=
cleanup_hook() {
    [ -n "$ZPARENT" ] && kill "$ZPARENT" 2>/dev/null
    [ -n "$SWAPDEV" ] && swapoff "$SWAPDEV" >/dev/null 2>&1
    [ -n "$DMNAME" ] && dmsetup remove "$DMNAME" >/dev/null 2>&1
    [ -n "$NSPID" ] && exec 9>&- 2>/dev/null
    [ -n "$NSPID" ] && wait "$NSPID" 2>/dev/null
    [ -n "$NLOOP" ] && lo_detach "$NLOOP"
    if [ -n "$NS" ] && [ -e "$NS/img" ]; then
        for d in $(losetup -j "$NS/img" -n -O NAME 2>/dev/null); do losetup -d "$d" 2>/dev/null; done
    fi
    [ -n "$NS" ] && rm -rf "$NS" 2>/dev/null
    return 0
}
"$B" new "$T" --yes >/dev/null 2>&1 || { echo "BAD  new 失败"; exit 1; }
"$B" create "$T" --size 48M >/dev/null 2>&1 || { echo "BAD  create 失败"; exit 1; }
L=$(lo_attach "$T") || { echo "BAD  losetup 失败"; exit 1; }
lo_waitpart "${L}p1" || { echo "SKIP: $L 没有分区节点"; exit 0; }
mkfs.ext4 -q "${L}p1" >/dev/null 2>&1 || { echo "BAD  mkfs.ext4 失败"; exit 1; }

echo "=== A: 身份编码与 fstat 一致"
if command -v python3 >/dev/null 2>&1; then
    python3 - "$T" "$L" <<'PY'
import fcntl, os, struct, sys
img, loop = sys.argv[1], sys.argv[2]
st = os.stat(img)
with open(loop, "rb") as f:
    buf = fcntl.ioctl(f.fileno(), 0x4C05, bytes(232))
dev, ino = struct.unpack_from("<QQ", buf, 0)
if (dev, ino) != (st.st_dev, st.st_ino):
    print(f"  BAD  lo=({dev},{ino}) fstat=({st.st_dev},{st.st_ino})")
    sys.exit(1)
print(f"  OK   lo=({dev},{ino}) == fstat")
PY
    exp "$?" 0 "lo_device/lo_inode 与 st_dev/st_ino 一致"

    FREE=$(losetup -f 2>/dev/null)
    if [ -n "$FREE" ] && [ -b "$FREE" ]; then
        python3 - "$FREE" <<'PY'
import errno, fcntl, os, sys
fd = os.open(sys.argv[1], os.O_RDONLY)
try:
    fcntl.ioctl(fd, 0x4C05, bytes(232))
except OSError as e:
    if e.errno != errno.ENXIO:
        print(f"  BAD  未绑定 loop 的 errno={e.errno}（want ENXIO={errno.ENXIO}）")
        sys.exit(1)
    print("  OK   未绑定 loop 的 ioctl 返回 ENXIO")
else:
    print("  BAD  未绑定 loop 返回了状态")
    sys.exit(1)
PY
        exp "$?" 0 "只有 ENXIO 表示未绑定"
    else
        echo "SKIP: 没有空闲 loop 设备"
    fi
else
    echo "SKIP: python3 缺失，无法读取 LOOP_GET_STATUS64"
fi

echo "=== B: backing 路径不可解析仍命中"
LNK="${T}.alias"
ln "$T" "$LNK" || { echo "BAD  硬链接失败"; exit 1; }
L2=$(lo_attach "$LNK") || { echo "BAD  从硬链接建 loop 失败"; exit 1; }
lo_waitpart "${L2}p1" || { echo "BAD  ${L2}p1 未就绪"; exit 1; }
mount_at "${L2}p1" /testmnt || { echo "BAD  挂载 ${L2}p1 失败"; exit 1; }
rm -f "$LNK"
[ ! -e "$LNK" ] || { echo "BAD  硬链接未删除"; exit 1; }
"$B" resize-part "$T:1" --start 2048 --grow-to-end >/dev/null 2>&1
exp "$?" 10 "硬链接删除后仍按身份命中"
unmount_now /testmnt
lo_detach "$L2"

echo "=== E: loop 分区被块设备栈持有"
DMNAME=
if command -v dmsetup >/dev/null 2>&1 && dmsetup version >/dev/null 2>&1; then
    # 独立的镜像：改表用缩小（Overwrite）形状，避免 Extend 的 dm-only 放行路径
    T2=$(mktemp /tmp/loopid-hold-XXXXXX.img)
    track_file "$T2"
    truncate -s 64M "$T2"
    "$B" new "$T2" --yes >/dev/null 2>&1 || { echo "BAD  new 失败"; exit 1; }
    "$B" create "$T2" --size 32M >/dev/null 2>&1 || { echo "BAD  create 失败"; exit 1; }
    L3=$(lo_attach "$T2") || { echo "BAD  losetup 失败"; exit 1; }
    if ! lo_waitpart "${L3}p1"; then
        echo "SKIP: ${L3}p1 未就绪"
    else
        DMNAME="diskedit-hold-$$"
        if dmsetup create "$DMNAME" --table "0 2048 linear ${L3}p1 0" >/dev/null 2>&1; then
            sleep 0.3
            out=$("$B" resize-part "$T2:1" --start 2048 --end 4095 2>&1)
            exp "$?" 10 "分区被 dm 持有：改表被拒"
            case "$out" in
                *"held by"*) echo "  OK   拒绝原因落在 holders 分支" ;;
                *) echo "  BAD  拒绝原因不是 holders: $out"; rc=1 ;;
            esac
            dmsetup remove "$DMNAME" >/dev/null 2>&1
            DMNAME=
            sleep 0.3
            # 同一判定的另一层：持有者挂在 loop 本身而不是分区
            DMNAME="diskedit-hold-$$"
            if dmsetup create "$DMNAME" --table "0 2048 linear ${L3} 0" >/dev/null 2>&1; then
                sleep 0.3
                out=$("$B" resize-part "$T2:1" --start 2048 --end 4095 2>&1)
                exp "$?" 10 "loop 本身被 dm 持有：改表被拒"
                case "$out" in
                    *"held by"*) echo "  OK   拒绝原因落在 holders 分支" ;;
                    *) echo "  BAD  拒绝原因不是 holders: $out"; rc=1 ;;
                esac
                dmsetup remove "$DMNAME" >/dev/null 2>&1
                DMNAME=
                sleep 0.3
            else
                echo "SKIP: dmsetup create（整盘 loop）不可用"
            fi
            # 解除持有后改走扩容；T2 没有文件系统，改表需显式 --no-fs
            "$B" resize-part "$T2:1" --start 2048 --grow-to-end --no-fs >/dev/null 2>&1
            exp "$?" 0 "解除持有后放行（阳性对照）"
        else
            echo "SKIP: dmsetup create 不可用"
        fi
        lo_detach "$L3"
    fi
else
    echo "SKIP: 没有可用的 device-mapper"
fi

echo "=== F: loop 分区是活动 swap"
SWAPDEV=
T3=$(mktemp /tmp/loopid-swap-XXXXXX.img)
track_file "$T3"
truncate -s 64M "$T3"
"$B" new "$T3" --yes >/dev/null 2>&1 || { echo "BAD  new 失败"; exit 1; }
"$B" create "$T3" --size 32M >/dev/null 2>&1 || { echo "BAD  create 失败"; exit 1; }
L4=$(lo_attach "$T3") || { echo "BAD  losetup 失败"; exit 1; }
if ! lo_waitpart "${L4}p1"; then
    echo "SKIP: ${L4}p1 未就绪"
elif mkswap "${L4}p1" >/dev/null 2>&1 && swapon "${L4}p1" 2>/dev/null; then
    SWAPDEV="${L4}p1"
    "$B" resize-part "$T3:1" --start 2048 --grow-to-end --no-fs >/dev/null 2>&1
    exp "$?" 10 "分区是活动 swap：改表被拒"
    swapoff "$SWAPDEV" >/dev/null 2>&1
    SWAPDEV=
    "$B" resize-part "$T3:1" --start 2048 --grow-to-end --no-fs >/dev/null 2>&1
    exp "$?" 0 "swapoff 后放行（阳性对照）"
else
    echo "SKIP: swapon 不可用"
fi
lo_detach "$L4"

echo "=== D: loop 设备节点不可访问"
if compgen -G "/sys/block/loop[0-9]*" >/dev/null; then
    out=$(unshare -m bash -c '
        mount --make-rprivate /
        mount -t tmpfs none /dev
        exec "$1" resize-part "$2:1" --start 2048 --grow-to-end
    ' _ "$B" "$T" 2>&1)
    got=$?
    exp "$got" 30 "loop 节点不可访问时退出码 30"
    case "$out" in
        *"cannot open /dev/loop"*) echo "  OK   错误正文指明无法打开 loop 节点" ;;
        *) echo "  BAD  错误正文缺少诊断: $out"; rc=1 ;;
    esac
else
    echo "SKIP: /sys/block 下没有 loop 设备"
fi

echo "=== C: 另一 mount namespace 内的挂载"
NS=$(mktemp -d /tmp/loopid-ns-XXXXXX)
: > "$NS/img"
FIFO="$NS/ctl"
mkfifo "$FIFO"
READY="$NS/ready"
mkdir -p /testmnt
# 子进程把镜像 bind 到只存在于本 namespace 的路径上，再建 loop 并挂载其分区；
# 挂载留在子进程的私有 namespace 里，直到 FIFO 关闭才退出
unshare -m bash -c '
    set -e
    mount --make-rprivate /
    mount --bind "$1" "$2/img"
    L=$(losetup -fP --show "$2/img")
    for _ in $(seq 1 50); do [ -b "${L}p1" ] && break; sleep 0.1; done
    mount "${L}p1" /testmnt
    printf "%s" "$L" > "$3"
    read -r _ < "$4"
' _ "$T" "$NS" "$READY" "$FIFO" &
NSPID=$!
exec 9<>"$FIFO"
for _ in $(seq 1 100); do [ -s "$READY" ] && break; sleep 0.1; done
if [ ! -s "$READY" ]; then
    echo "BAD  namespace 未就绪"
    exit 1
fi
NLOOP=$(cat "$READY")
"$B" resize-part "$T:1" --start 2048 --grow-to-end >/dev/null 2>&1
exp "$?" 10 "另一 namespace 的挂载被检出"
exec 9>&-
wait "$NSPID" 2>/dev/null
NSPID=
sleep 0.3
"$B" resize-part "$T:1" --start 2048 --grow-to-end >/dev/null 2>&1
exp "$?" 0 "namespace 结束后放行（阳性对照）"
lo_detach "$NLOOP"
NLOOP=
rm -rf "$NS"
NS=

echo "=== G: 进程退出竞态"
if ! command -v python3 >/dev/null 2>&1; then
    echo "SKIP: python3 缺失"
else
    ZFILE="$T.zpid"
    # 父进程 fork 后不回收子进程：僵尸的 nsproxy 已在退出时清空，
    # 内核对它的 mountinfo 打开请求返回 EINVAL（fs/proc_namespace.c）
    python3 - "$ZFILE" <<'PY' &
import os, sys, time
pid = os.fork()
if pid == 0:
    os._exit(0)
open(sys.argv[1], "w").write(str(pid))
time.sleep(120)
PY
    ZPARENT=$!
    for _ in $(seq 1 50); do [ -s "$ZFILE" ] && break; sleep 0.1; done
    ZPID=$(cat "$ZFILE" 2>/dev/null)
    if [ -z "$ZPID" ] || [ ! -d "/proc/$ZPID" ]; then
        echo "SKIP: 无法构造已退出未回收的进程"
    else
        python3 - "$ZPID" <<'PY'
import errno, sys
try:
    open(f"/proc/{sys.argv[1]}/mountinfo", "rb").read()
except OSError as e:
    if e.errno != errno.EINVAL:
        print(f"  BAD  mountinfo errno={e.errno}（want EINVAL）")
        sys.exit(1)
    print("  OK   已退出进程的 mountinfo 返回 EINVAL")
    sys.exit(0)
print("  BAD  已退出进程的 mountinfo 仍可读")
sys.exit(1)
PY
        exp "$?" 0 "内核以 EINVAL 表示该进程已失去挂载命名空间"

        T5=$(mktemp /tmp/loopid-race-XXXXXX.img)
        track_file "$T5"
        truncate -s 64M "$T5"
        "$B" new "$T5" --yes >/dev/null 2>&1 || { echo "BAD  new 失败"; exit 1; }
        "$B" create "$T5" --size 32M >/dev/null 2>&1 || { echo "BAD  create 失败"; exit 1; }
        "$B" resize-part "$T5:1" --start 2048 --grow-to-end --no-fs >/dev/null 2>&1
        exp "$?" 0 "同一窗口内正常写盘不被误拒"

        T6=$(mktemp /tmp/loopid-race2-XXXXXX.img)
        track_file "$T6"
        truncate -s 64M "$T6"
        "$B" new "$T6" --yes >/dev/null 2>&1 || { echo "BAD  new 失败"; exit 1; }
        "$B" create "$T6" --size 32M >/dev/null 2>&1 || { echo "BAD  create 失败"; exit 1; }
        out=$(unshare -m bash -c '
            mount --make-rprivate /
            mount -t tmpfs none /dev
            exec "$1" resize-part "$2:1" --start 2048 --grow-to-end --no-fs
        ' _ "$B" "$T6" 2>&1)
        exp "$?" 30 "真实探测故障不被 EINVAL 跳过掩盖"
        case "$out" in
            *"cannot open /dev/loop"*) echo "  OK   拒绝原因仍是无法打开 loop 节点" ;;
            *) echo "  BAD  拒绝原因不符: $out"; rc=1 ;;
        esac
    fi
    [ -n "$ZPARENT" ] && kill "$ZPARENT" 2>/dev/null
    [ -n "$ZPARENT" ] && wait "$ZPARENT" 2>/dev/null
    ZPARENT=
    rm -f "$ZFILE"
fi

echo "=== G2: 权限不足不等于进程已退出"
if ! command -v setpriv >/dev/null 2>&1; then
    echo "SKIP: setpriv 缺失"
else
    T8=$(mktemp /tmp/loopid-perm-XXXXXX.img)
    track_file "$T8"
    truncate -s 64M "$T8"
    "$B" new "$T8" --yes >/dev/null 2>&1 || { echo "BAD  new 失败"; exit 1; }
    # 降权进程要能读写目标；mountinfo 的可见性由 hidepid 决定，与目标权限无关
    chmod 666 "$T8"
    # 伴随文件由 root 的 `new` 建出；降权进程要能拿到写锁，否则测的就不是 mountinfo
    for f in "$T8"$SIDECAR_GLOB; do
        [ -e "$f" ] && chmod 666 "$f"
    done
    out=$(unshare -m bash -c '
        mount --make-rprivate /
        mount -t proc -o hidepid=1 proc /proc || exit 99
        exec setpriv --reuid=65534 --regid=65534 --clear-groups "$1" new "$2" --yes
    ' _ "$B" "$T8" 2>&1)
    got=$?
    if [ "$got" -eq 99 ]; then
        echo "SKIP: 无法以 hidepid=1 挂载 procfs"
    else
        exp "$got" 30 "读不到他人 mountinfo 时拒绝写盘"
        case "$out" in
            *"cannot read /proc/"*"mountinfo"*) echo "  OK   失败原因落在 mountinfo 读取（未被当成进程退出跳过）" ;;
            *) echo "  BAD  失败原因不符: $out"; rc=1 ;;
        esac
    fi
fi

echo "=== H: 多个 loop 命中时的持有者归属"
if ! command -v dmsetup >/dev/null 2>&1 || ! dmsetup version >/dev/null 2>&1; then
    echo "SKIP: 没有可用的 device-mapper"
else
    T9=$(mktemp /tmp/loopid-attr-XXXXXX.img)
    track_file "$T9"
    truncate -s 64M "$T9"
    "$B" new "$T9" --yes >/dev/null 2>&1 || { echo "BAD  new 失败"; exit 1; }
    "$B" create "$T9" --size 32M >/dev/null 2>&1 || { echo "BAD  create 失败"; exit 1; }
    LA=$(lo_attach "$T9") || { echo "BAD  losetup 失败"; exit 1; }
    LB=$(lo_attach "$T9") || { echo "BAD  losetup 失败"; exit 1; }
    if ! lo_waitpart "${LA}p1" || ! lo_waitpart "${LB}p1"; then
        echo "SKIP: 分区节点未就绪"
    else
        DMNAME="diskedit-attr-$$"
        if dmsetup create "$DMNAME" --table "0 2048 linear ${LB}p1 0" >/dev/null 2>&1; then
            sleep 0.3
            out=$("$B" new "$T9" --yes 2>&1)
            exp "$?" 10 "两个 loop 命中、其一被持有：改表被拒"
            LAB=${LA##*/}
            LBB=${LB##*/}
            # sysfs holders 里记录的是内核名 dm-N，不是 dmsetup 的映射名
            case "$out" in
                *"/dev/$LBB"*"held by dm-"*) echo "  OK   持有者归到实际被持有的 /dev/$LBB" ;;
                *) echo "  BAD  归属错误（期望 /dev/$LBB + dm-N）: $out"; rc=1 ;;
            esac
            case "$out" in
                *"/dev/$LAB"*) echo "  BAD  消息指向了未被持有的 /dev/$LAB: $out"; rc=1 ;;
                *) echo "  OK   未被持有的 /dev/$LAB 不出现在拒绝消息中" ;;
            esac
            dmsetup remove "$DMNAME" >/dev/null 2>&1
            DMNAME=
            sleep 0.3
            "$B" new "$T9" --yes >/dev/null 2>&1
            exp "$?" 0 "解除持有后改表放行（阳性对照）"
        else
            echo "SKIP: dmsetup create 不可用"
        fi
        lo_detach "$LB"
        lo_detach "$LA"
    fi
fi

exit $rc