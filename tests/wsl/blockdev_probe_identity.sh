#!/usr/bin/env bash
# 真实块设备的平台识别：HostPlatform::identify 对块设备节点给出 Block 介质、
# 内核报告的扇区大小与设备号身份。缺 root / losetup / cargo 时明确失败，不跳过。
source "$(dirname "$0")/lib.sh"

missing() { echo "BAD: $*"; exit 1; }
[ "$(id -u)" -eq 0 ] || missing "requires root for losetup"
command -v losetup >/dev/null || missing "losetup not installed"
# sudo 会把 PATH/HOME 换成 root 的；rustup 装在调用者家目录时，沿用他的 cargo 环境
if ! command -v cargo >/dev/null; then
    _u=${SUDO_USER:-$(id -un)}
    _h=$(getent passwd "$_u" | cut -d: -f6)
    if [ -n "$_h" ] && [ -x "$_h/.cargo/bin/cargo" ]; then
        PATH="$_h/.cargo/bin:$PATH"
        CARGO_HOME=${CARGO_HOME:-"$_h/.cargo"}
        RUSTUP_HOME=${RUSTUP_HOME:-"$_h/.rustup"}
        export PATH CARGO_HOME RUSTUP_HOME
    fi
fi
command -v cargo >/dev/null || missing "cargo not installed"

T=$(mktemp "$WSL_ROOT/blockdev-probe-XXXXXX.img")
truncate -s 16M "$T" || missing "cannot create $T"
track_file "$T"
L=$(lo_attach "$T") || missing "cannot attach $T to a loop device"

cd "$_repo_root" || exit 1
out=$(DISKEDIT_BLOCK_DEVICE="$L" CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/diskedit-test-target}" \
    cargo test --bin DiskEdit platform::probe::tests::identifies_a_real_block_device \
    -- --ignored --exact --nocapture 2>&1)
rc=$?
printf '%s\n' "$out"
case "$out" in
    *"test platform::probe::tests::identifies_a_real_block_device ... ok"*"1 passed"*)
        echo "  OK   $L is identified as a kernel-reported block device" ;;
    *)
        echo "  BAD  the ignored test did not run to a pass"
        rc=1 ;;
esac
exp "$rc" 0 "真实块设备的平台识别"
exit "$rc"
