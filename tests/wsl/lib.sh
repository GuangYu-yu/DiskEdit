#!/usr/bin/env bash
# tests/wsl 共享支撑：断言、二进制定位、loop/挂载/临时文件的生命周期。
# 由各测试脚本 source；本文件自身不执行测试。
#
# 二进制默认取 <repo>/tmp 下的 Linux 交叉编译产物（宿主 zigbuild 的输出落点），
# 可用 DISKEDIT_BIN_DIR 覆盖，或由脚本以第一个参数给出目录。
# 约定：B=普通版、BF=test-faults 版；只有需要故障注入的脚本才要求 BF 存在。
#
# 脚本用法：
#   source "$(dirname "$0")/lib.sh"
#   require_bin                      # 或 require_fault_bin
#   exp "$?" 0 "label"               # 断言退出码
#   LD=$(lo_attach "$T")             # 建 loop(-P) 并登记，退出时自动收
#   mount_at "${LD}p1" /testmnt      # 挂载并登记
#   track_file "$T"                  # 登记临时镜像（含 .diskedit.* 伴随文件）
#   cleanup_hook() { ... }           # 可选：脚本自有的额外收尾（退出时先跑）
#   结尾 exit $rc

_wsl_self=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
_repo_root=$(cd "$_wsl_self/../.." && pwd)

_bin_dir=${1:-${DISKEDIT_BIN_DIR:-$_repo_root/tmp}}
B=$_bin_dir/DiskEdit
BF=$_bin_dir/DiskEdit-fault

# 落盘伴随文件后缀：与 src/dev.rs 的词表同名同值，改一处要一起改。
# SIDECAR_GLOB 是"整族伴随文件"的清理通配（故意不含引号，靠 shell 路径展开）；
# LEGACY_CKPT_SUFFIX 是旧版按 GPT Disk GUID 命名的 checkpoint 后缀。
JOURNAL_SUFFIX=.diskedit.journal
CKPT_SUFFIX=.diskedit.ckpt
LOCK_SUFFIX=.diskedit.lock
SIDECAR_GLOB=.diskedit.*
LEGACY_CKPT_SUFFIX=.ckpt

rc=0

require_bin() {
    if [ ! -x "$B" ]; then
        echo "diskedit 二进制不存在或不可执行: $B" >&2
        echo "在宿主上构建：cargo zigbuild --release --target x86_64-unknown-linux-gnu" >&2
        exit 1
    fi
}

require_fault_bin() {
    require_bin
    if [ ! -x "$BF" ]; then
        echo "test-faults 二进制不存在或不可执行: $BF" >&2
        echo "在宿主上构建：cargo zigbuild --release --target x86_64-unknown-linux-gnu --features test-faults" >&2
        exit 1
    fi
}

# got want label：退出码断言
exp() {
    if [ "$1" = "$2" ]; then
        echo "  OK   $3 (exit=$1)"
    else
        echo "  BAD  $3 (got $1, want $2)"
        rc=1
        return 1
    fi
}

# got want label：取值断言（几何/尺寸/哈希等非退出码事实）
assert_eq() {
    if [ "$1" = "$2" ]; then
        echo "  OK   $3 ($1)"
    else
        echo "  BAD  $3 (got $1, want $2)"
        rc=1
        return 1
    fi
}

# 断言机制自测（meta）：验证 exp/assert_eq 的通过与失败两条路径。
# 失败路径必须"返回非零且不打印 OK"——否则断言悄悄通过、测试全绿而实则没测。
# 由 selftest.sh 调用；全部正确时返回 0（不触碰 rc 与任何文件）
selftest_assertions() {
    local bad=0 out r
    out=$(exp 0 0 "selftest/exp-match"); r=$?
    [ "$r" -eq 0 ] || { echo "SELFTEST BAD: exp(匹配) 返回 rc=$r（want 0）"; bad=1; }
    case "$out" in *OK*) ;; *) echo "SELFTEST BAD: exp(匹配) 未打印 OK"; bad=1;; esac

    out=$(exp 0 7 "selftest/exp-mismatch"); r=$?
    [ "$r" -ne 0 ] || { echo "SELFTEST BAD: exp(不匹配) 返回 rc=0（want 非零）"; bad=1; }
    case "$out" in *OK*) echo "SELFTEST BAD: exp(不匹配) 打印了 OK"; bad=1;; esac
    case "$out" in *BAD*) ;; *) echo "SELFTEST BAD: exp(不匹配) 未打印 BAD"; bad=1;; esac

    out=$(assert_eq a a "selftest/eq-match"); r=$?
    [ "$r" -eq 0 ] || { echo "SELFTEST BAD: assert_eq(匹配) 返回 rc=$r（want 0）"; bad=1; }
    case "$out" in *OK*) ;; *) echo "SELFTEST BAD: assert_eq(匹配) 未打印 OK"; bad=1;; esac

    out=$(assert_eq a b "selftest/eq-mismatch"); r=$?
    [ "$r" -ne 0 ] || { echo "SELFTEST BAD: assert_eq(不匹配) 返回 rc=0（want 非零）"; bad=1; }
    case "$out" in *OK*) echo "SELFTEST BAD: assert_eq(不匹配) 打印了 OK"; bad=1;; esac
    case "$out" in *BAD*) ;; *) echo "SELFTEST BAD: assert_eq(不匹配) 未打印 BAD"; bad=1;; esac
    return "$bad"
}

# ---- 资源生命周期 ----
_TRACK_LOOPS=()
_TRACK_MOUNTS=()
_TRACK_FILES=()

track_file() { _TRACK_FILES+=("$1"); }

# 以 -P 建 loop，外部参数透传（如 --sector-size 4096、-r）。
# 注意：必须用 `LD=$(lo_attach "$T")` 取设备名——命令替换在子 shell 里跑，
# 因此 loop 不在登记表中，退出时由 cleanup_all 按镜像反查释放（losetup -j）。
lo_attach() {
    local img=$1; shift
    losetup -fP "$@" --show "$img"
}

# 等待分区设备节点就绪：losetup -P 的分区扫描是异步的，节点可能在 attach
# 返回后短暂不可读；attach 后立即访问分区设备的调用点必须先过这一关
lo_waitpart() {
    local dev=$1
    for _ in {1..20}; do
        [ -b "$dev" ] && blockdev --getsize64 "$dev" >/dev/null 2>&1 && return 0
        sleep 0.1
    done
    return 1
}

mount_at() { # dev mp [mount 选项...]
    local dev=$1 mp=$2; shift 2
    mkdir -p "$mp" || return 1
    mount "$@" "$dev" "$mp" || return 1
    _TRACK_MOUNTS+=("$mp")
}

# 中途释放：立即卸载并从登记表移除（脚本需要重新挂载或验证卸载后状态时用）
unmount_now() {
    local mp=$1 i
    umount "$mp" 2>/dev/null
    if [ ${#_TRACK_MOUNTS[@]} -gt 0 ]; then
        for i in "${!_TRACK_MOUNTS[@]}"; do
            [ "${_TRACK_MOUNTS[$i]}" = "$mp" ] && unset "_TRACK_MOUNTS[$i]"
        done
    fi
}

# 中途释放：立即 detach 并从登记表移除（脚本需要重新 attach 该镜像时用）
lo_detach() {
    local dev=$1 i
    losetup -d "$dev" 2>/dev/null
    if [ ${#_TRACK_LOOPS[@]} -gt 0 ]; then
        for i in "${!_TRACK_LOOPS[@]}"; do
            [ "${_TRACK_LOOPS[$i]}" = "$dev" ] && unset "_TRACK_LOOPS[$i]"
        done
    fi
}

# 运行前复位：上一次 kill -9 可能留下挂载态，直接挂载会 busy、取值会取到旧状态
reset_mounts() {
    local mp
    for mp in "$@"; do umount "$mp" 2>/dev/null; done
}

cleanup_all() {
    local x d
    for x in ${_TRACK_MOUNTS[@]+"${_TRACK_MOUNTS[@]}"}; do umount "$x" 2>/dev/null; done
    # loop 由「镜像 → 关联设备」反查释放，覆盖命令替换建出的 loop（登记表收不到）
    for x in ${_TRACK_FILES[@]+"${_TRACK_FILES[@]}"}; do
        for d in $(losetup -j "$x" -n -O NAME 2>/dev/null); do losetup -d "$d" 2>/dev/null; done
    done
    for x in ${_TRACK_LOOPS[@]+"${_TRACK_LOOPS[@]}"}; do losetup -d "$x" 2>/dev/null; done
    # 额外收尾放在资源释放之后：hook 常依赖「已卸载/loop 已释放」的前置
    declare -F cleanup_hook >/dev/null && cleanup_hook
    for x in ${_TRACK_FILES[@]+"${_TRACK_FILES[@]}"}; do
        rm -f "$x" "$x"$SIDECAR_GLOB 2>/dev/null
    done
}
trap cleanup_all EXIT

# ---- 运行前复位（source 时执行一次）----
# 上一次 kill -9 可能留下挂载态：本次 mount 会叠在旧挂载之上、取值会取到旧状态而假报 CORRUPT。
# 这里一次到位地复位本套测试专用的挂载点（/testmnt），调用点无需各自记得。
# 幂等；只 umount 这一个已知挂载点，绝不动任何 loop（不 losetup -D），避免误伤他人在用的映射
reset_mounts /testmnt