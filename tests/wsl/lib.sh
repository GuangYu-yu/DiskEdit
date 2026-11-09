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
#   img T t24.img                    # 根下建镜像路径并登记（登记留在本 shell）
#   exp "$?" 0 "label"               # 断言退出码
#   LD=$(lo_attach "$T")             # 建 loop(-P) 并登记，退出时自动收
#   mount_at "${LD}p1" "$TEST_MNT"   # 挂载并登记（挂载点必须在 WSL_ROOT 内）
#   cleanup_hook() { ... }           # 可选：脚本自有的额外收尾（退出时先跑）
#   结尾 exit $rc
#
# 残留控制（唯一约定）：本套测试的一切产物——镜像、快照、e2fsck 输出、挂载点、
# 块设备侧的记录与锁——都落在 WSL_ROOT 之内，WSL 之外不新增任何路径。
# 块设备侧的记录/锁由产品的 DISKEDIT_STATE_DIR 接口重定向进根（见 state_dir()）；
# 要覆盖默认落点的行为本身时，脚本显式调 use_default_state_dir 并自带清理。
# 退出路径：EXIT trap 释放资源并整根删除；INT/TERM/HUP 先回收再退出。
# 只有 SIGKILL（timeout -k 兜底、CI 取消）绕过 trap——留下的孤儿根在下次
# source 时按「PID 不存活」清扫，挂载点一并卸载，不会污染 WSL 之外。

_wsl_self=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
_repo_root=$(cd "$_wsl_self/../.." && pwd)

_bin_dir=${1:-${DISKEDIT_BIN_DIR:-$_repo_root/tmp}}
B=$_bin_dir/DiskEdit
BF=$_bin_dir/DiskEdit-fault
# 假工具场景用的两个变体：工具表在编译期钉死（src/fsops/env.rs 的 TOOL_PATH），
# 调用环境的 PATH 不参与解析，所以"工具失败/缺失"只能靠换掉表来实现
#   BFK  表 = 假工具目录 + 系统目录：假工具优先，其余照常
#   BFKO 表 = 只有假工具目录：脚本把所需真工具 symlink 进去，缺的就是真的缺
FAKE_TOOL_DIR=/var/tmp/diskedit-fakebin
BFK=$_bin_dir/DiskEdit-fake
BFKO=$_bin_dir/DiskEdit-fakeonly

# ---- 残留的唯一根 ----
# 带 PID：并行跑两套回归互不踩；退出整根删除，残留严格为零。
WSL_ROOT=${DISKEDIT_WSL_ROOT:-/var/tmp/diskedit-wsl.$$}
mkdir -p "$WSL_ROOT" 2>/dev/null || { echo "cannot create WSL_ROOT=$WSL_ROOT" >&2; exit 1; }
TEST_MNT=$WSL_ROOT/mnt/testmnt
# 块设备侧的记录与锁收进根（src/dev/identity.rs 的 state_dir() 读这个变量）。
use_default_state_dir() { export DISKEDIT_STATE_DIR=/var/lib/diskedit; }
export DISKEDIT_STATE_DIR=$WSL_ROOT/state

# img <变量名> <name>：把根下产物路径赋给指定变量并登记清理。所有落盘文件都
# 从这里取路径，不得直接写 /var/tmp、/tmp 或仓库目录。按变量名赋值而不是命令
# 替换回显——`T=$(img x)` 会让登记发生在子 shell 里，父 shell 收不到，loop 反查
# 就会漏（与 lo_attach 的注释同一坑）
img() {
    local __var=$1
    local p=$WSL_ROOT/$2
    track_file "$p"
    printf -v "$__var" '%s' "$p"
}

# 落盘伴随文件后缀：与 src/dev/identity.rs 的词表同名同值，改一处要一起改。
# 命名空间只写一次，各后缀由它拼出。
# SIDECAR_GLOB 是"整族伴随文件"的清理通配（故意不含引号，靠 shell 路径展开）：
# `*` 而不是 `.*`，因为记录本身落在目录 <img>.diskedit/ 里，清理要用 rm -rf。
NS=.diskedit
LOCK_SUFFIX=${NS}.lock
SIDECAR_GLOB=${NS}*

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

require_fake_bin() {
    require_bin
    if [ ! -x "$BFK" ]; then
        echo "工具表变体二进制不存在: $BFK" >&2
        echo "它由 wsl-tests.yml 以 DISKEDIT_TOOL_PATH=\"$FAKE_TOOL_DIR:<系统目录>\" 构建" >&2
        exit 1
    fi
}

require_fakeonly_bin() {
    require_fake_bin
    if [ ! -x "$BFKO" ]; then
        echo "工具表变体二进制不存在: $BFKO" >&2
        echo "它由 wsl-tests.yml 以 DISKEDIT_TOOL_PATH=\"$FAKE_TOOL_DIR\" 构建" >&2
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

# 分区摘要，供前后对比。读失败时不返回空串（空串会让 assert_eq 报"数据不一致"，
# 把节点消失或 I/O 错误说成产品写坏数据），而是返回带原因的值，比较时一眼可见
mdsum() {
    local out
    if ! out=$(md5sum "$1" 2>&1); then
        printf 'unreadable: %s\n' "$out"
        return 0
    fi
    printf '%s\n' "${out%% *}"
}

# ---- 恢复记录现场 ----
# 落点与进度都从 inspect 的输出里取：目录名/文件名只在 src/dev/identity.rs 定义，
# 脚本不复刻路径规则。未收尾时 inspect 退 5，命令替换里不影响后续判断。
record_file() {
    "$B" inspect "$1" 2>/dev/null | sed -n 's/^evidence: //p' | head -1
}

# 有收尾未完的记录（可续跑或待收尾都算），且结构可读
record_present() {
    "$B" inspect "$1" 2>/dev/null | grep -q '^record: recognized$'
}

# 目标上没有记录
record_absent() {
    "$B" inspect "$1" 2>/dev/null | grep -q '^record: absent$'
}

# 记录里的搬移游标：已完成的块数
record_cursor() {
    "$B" inspect "$1" 2>/dev/null | sed -n 's/^  progress: .*cursor=\([0-9]*\).*/\1/p' | head -1
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
# 反查按 backing 路径匹配：镜像改名或先被删除（如硬链接场景）就找不到该 loop，
# 它会留到 `losetup -a` 手动清理；退出路径被 KILL 时清理同样不执行
lo_attach() {
    local img=$1; shift
    local dev
    dev=$(losetup -fP "$@" --show "$img") || return 1
    # -P 的分区扫描不保证节点已由 udev 物化：attach 后立即重触发一次读表，
    # 让内核分区视图与设备节点在返回时尽量就位（未就绪的残余由 lo_waitpart 兜底）
    partx -u "$dev" >/dev/null 2>&1
    printf '%s\n' "$dev"
}

# 等待分区设备节点就绪：losetup -P 的分区扫描是异步的，节点可能在 attach
# 返回后短暂不可读；attach 后立即访问分区设备的调用点必须先过这一关。
# 前半程只等，后半程重触发读表并补一次 udev 收尾——runner 上 udev 迟到是常态
lo_waitpart() {
    local dev=$1
    for _ in {1..100}; do
        [ -b "$dev" ] && blockdev --getsize64 "$dev" >/dev/null 2>&1 && return 0
        if [ "$_" = 50 ] && [ -b "${dev%p[0-9]*}" ]; then
            partx -u "${dev%p[0-9]*}" 2>/dev/null
            udevadm settle 2>/dev/null
        fi
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
    # 整根兜底：个别脚本漏 track 的文件也一并消失，残留严格为零
    rm -rf "$WSL_ROOT" 2>/dev/null
}

# 运行前清扫：SIGKILL 绕过了上一轮的 EXIT trap，它留下的孤儿根里可能有挂载点
# 与 loop。只处理「PID 已不存在」的根；并行运行的另一套回归（根内 PID 存活）不动。
# 挂载点先卸载（挂在孤儿根里的设备会让整盘 busy），loop 按 backing 在根内的镜像反查释放
sweep_orphan_roots() {
    local d pid mp dev img
    for d in /var/tmp/diskedit-wsl.*; do
        [ -d "$d" ] || continue
        pid=${d##*.}
        case "$pid" in ''|*[!0-9]*) continue ;; esac
        kill -0 "$pid" 2>/dev/null && continue
        for mp in $(awk '$2 ~ /^\/var\/tmp\/diskedit-wsl\./ {print $2}' /proc/self/mountinfo 2>/dev/null); do
            umount "$mp" 2>/dev/null
        done
        for img in "$d"/*.img "$d"/*/*.img; do
            [ -e "$img" ] || continue
            for dev in $(losetup -j "$img" -n -O NAME 2>/dev/null); do losetup -d "$dev" 2>/dev/null; done
        done
        umount "$d/mnt/testmnt" 2>/dev/null
        rm -rf "$d" 2>/dev/null
    done
}
sweep_orphan_roots

# 清理 trap：正常退出、显式 exit、INT/TERM/HUP 都先回收再走。
# INT/TERM/HUP 的 handler 先复位自身再 exit，让 EXIT trap 接管清理，
# 避免「信号处理函数里再次收到信号」造成的半清理状态
_on_signal() {
    trap - INT TERM HUP
    exit 130
}
trap _on_signal INT TERM HUP
trap cleanup_all EXIT