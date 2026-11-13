#!/usr/bin/env bash
# Linux 平台层校验：只在脚本自建的 loop 设备与临时镜像上执行，结束时释放设备并校验归属。
# 每项给出一种状态：
#   真跑过   平台层实际执行，按退出码或用例结果判定
#   靠脚本   脚本读内核事实，与产品输出对照后判定
#   环境阻塞 缺 root / losetup / cargo 等前置，无法给出结论
# 退出码：0 全部通过；1 有检查失败；2 环境阻塞。
set -u

_repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
_target=${CARGO_TARGET_DIR:-/tmp/diskedit-test-target}
_bin=$_target/debug/DiskEdit
_root=
_img=
_loop=
_rc=0

_items=(
    "block-identity|内核报告的块设备身份（sysfs 设备号与设备 ID）"
    "block-geometry|块设备的扇区大小与容量与内核读数一致"
    "block-sector-override|块设备拒绝 --sector-size 覆盖"
    "block-write|块设备上的表写命令（new/add/delete）"
)

verdict() { # 状态 名称 结论 [说明]
    printf '[%s] %s: %s\n' "$1" "$2" "$3"
    if [ $# -ge 4 ] && [ -n "$4" ]; then
        printf '        %s\n' "$4"
    fi
}

blocked_all() {
    local item
    for item in "${_items[@]}"; do
        verdict 环境阻塞 "${item%%|*}" 未执行 "${item#*|}；$1"
    done
    exit 2
}

# 只认由 losetup -j 按本脚本镜像反查出来的 loop 设备；未通过校验的一律不释放
loop_backed_by() {
    local dev
    for dev in $(losetup -j "$2" -n -O NAME 2>/dev/null); do
        [ "$dev" = "$1" ] && return 0
    done
    return 1
}

cleanup() {
    local dev remaining status=$?
    trap - EXIT
    [ "$status" -ne 0 ] && _rc=$status
    if [ -n "$_loop" ]; then
        if loop_backed_by "$_loop" "$_img"; then
            losetup -d "$_loop" 2>/dev/null || { printf 'BAD: 无法释放 %s\n' "$_loop" >&2; _rc=1; }
        else
            printf 'BAD: %s 的 backing file 不再是 %s，未释放\n' "$_loop" "$_img" >&2
            _rc=1
        fi
    fi
    if [ -n "$_img" ]; then
        for dev in $(losetup -j "$_img" -n -O NAME 2>/dev/null); do
            loop_backed_by "$dev" "$_img" && losetup -d "$dev" 2>/dev/null
        done
        remaining=$(losetup -j "$_img" -n -O NAME 2>/dev/null)
        if [ -n "$remaining" ]; then
            printf 'BAD: 清理后仍有 loop 指向 %s: %s\n' "$_img" "$remaining" >&2
            _rc=1
        fi
    fi
    if [ -n "$_root" ]; then
        rm -rf -- "$_root"
        if [ -e "$_root" ]; then
            printf 'BAD: 临时目录 %s 未删除\n' "$_root" >&2
            _rc=1
        fi
    fi
    exit "$_rc"
}

# 表提交后让内核分区视图追上盘上事实；命令不可用时静默略过
refresh_kernel() {
    sync
    blockdev --rereadpt "$_loop" >/dev/null 2>&1
    partx -u "$_loop" >/dev/null 2>&1
    command -v udevadm >/dev/null 2>&1 && udevadm settle >/dev/null 2>&1
    return 0
}

# 取 JSON 里第一个匹配字段的值
json_field() { # 字段 文本
    printf '%s\n' "$2" | grep -o "\"$1\":[0-9][0-9]*" | head -n 1 | cut -d: -f2
}

[ "$(id -u)" -eq 0 ] || blocked_all "需要 root 以创建 loop 设备"
for c in losetup truncate blockdev cargo; do
    command -v "$c" >/dev/null 2>&1 || blocked_all "缺少命令 $c"
done

_root=$(mktemp -d "${TMPDIR:-/tmp}/diskedit-linux-device.XXXXXX") || { printf 'BAD: 无法创建临时目录\n' >&2; exit 1; }
trap cleanup EXIT
_img=$_root/disk.img
export DISKEDIT_STATE_DIR=$_root/state
truncate -s 16M "$_img" || { printf 'BAD: 无法创建临时镜像 %s\n' "$_img" >&2; exit 1; }
_loop=$(losetup --find --partscan --show "$_img") || { printf 'BAD: 无法把 %s 关联到 loop 设备\n' "$_img" >&2; exit 1; }
if ! loop_backed_by "$_loop" "$_img"; then
    printf 'BAD: %s 的 backing file 不是 %s，拒绝在它上面执行任何操作\n' "$_loop" "$_img" >&2
    _loop=
    exit 1
fi
printf '临时镜像 %s -> loop %s\n' "$_img" "$_loop"

printf '构建 %s（CARGO_TARGET_DIR=%s）\n' "$_repo_root" "$_target"
if ! ( cd "$_repo_root" && CARGO_TARGET_DIR="$_target" cargo build --bin DiskEdit ); then
    printf 'BAD: cargo build --bin DiskEdit 非零退出，后续检查未执行\n' >&2
    exit 1
fi

out=$( cd "$_repo_root" && DISKEDIT_BLOCK_DEVICE="$_loop" CARGO_TARGET_DIR="$_target" \
    cargo test --bin DiskEdit platform::probe::tests::identifies_a_real_block_device \
    -- --ignored --exact --nocapture 2>&1 )
code=$?
if [ "$code" -eq 0 ] && printf '%s\n' "$out" | grep -q 'test result: ok\. 1 passed'; then
    verdict 真跑过 block-identity 通过
else
    verdict 真跑过 block-identity 失败 "cargo test 退出码 $code"
    printf '%s\n' "$out" | tail -n 20 | sed 's/^/        /'
    _rc=1
fi

out=$("$_bin" info "$_loop" 2>&1)
code=$?
kernel_ss=$(blockdev --getss "$_loop" 2>/dev/null)
kernel_bytes=$(blockdev --getsize64 "$_loop" 2>/dev/null)
prod_ss=$(json_field sector_size "$out")
prod_bytes=$(json_field size_bytes "$out")
if [ "$code" -eq 0 ] && [ -n "$kernel_ss" ] && [ -n "$kernel_bytes" ] \
    && [ "$prod_ss" = "$kernel_ss" ] && [ "$prod_bytes" = "$kernel_bytes" ]; then
    verdict 靠脚本 block-geometry 通过 "产品 $prod_ss/$prod_bytes 字节，内核 $kernel_ss/$kernel_bytes 字节"
else
    verdict 靠脚本 block-geometry 失败 "退出码 $code，产品 $prod_ss/$prod_bytes，内核 $kernel_ss/$kernel_bytes"
    printf '%s\n' "$out" | tail -n 5 | sed 's/^/        /'
    _rc=1
fi

out=$("$_bin" info "$_loop" --sector-size 4096 2>&1)
code=$?
if [ "$code" -eq 3 ]; then
    verdict 真跑过 block-sector-override 通过
else
    verdict 真跑过 block-sector-override 失败 "退出码 $code（期望 3）"
    printf '%s\n' "$out" | tail -n 5 | sed 's/^/        /'
    _rc=1
fi

failed_step=
out=$("$_bin" new "$_loop" --table gpt --yes 2>&1)
code=$?
if [ "$code" -ne 0 ]; then
    failed_step="new 退出码 $code"
else
    refresh_kernel
    out=$("$_bin" add "$_loop" 1M --yes 2>&1)
    code=$?
    if [ "$code" -ne 0 ]; then
        failed_step="add 退出码 $code"
    else
        refresh_kernel
        out=$("$_bin" delete "$_loop:1" --yes 2>&1)
        code=$?
        [ "$code" -eq 0 ] || failed_step="delete 退出码 $code"
    fi
fi
if [ -z "$failed_step" ]; then
    verdict 真跑过 block-write 通过
else
    verdict 真跑过 block-write 失败 "$failed_step"
    printf '%s\n' "$out" | tail -n 5 | sed 's/^/        /'
    _rc=1
fi

if [ "$_rc" -eq 0 ]; then
    printf '全部检查通过\n'
else
    printf '存在失败项\n' >&2
fi
exit "$_rc"
