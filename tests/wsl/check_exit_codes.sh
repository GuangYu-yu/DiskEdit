#!/usr/bin/env bash
# `check` 的验收载体。
#
# 四码：工具退出 0 → 0；工具非零 → 3；工具不在 PATH 或路径不存在 → 6；
# 已找到但启动失败 → 4。工具表的搜索目录在编译期钉死（src/fsops/env.rs 的 TOOL_PATH），
# 调用环境的 PATH 不参与解析，所以后两条只能换表来造：
#   BFKO 的表只有假工具目录 → 把 losetup 等真工具 symlink 进去，e2fsck 真的缺失 → 6
#   BFK  的表是假目录 + 系统目录 → 假目录里放带 x 位的非可执行文件 → 启动失败 → 4
#
# 本脚本钉住工具侧三条；非工具路径的三条（缺 :N 的 2、空槽的 3、缺文件系统的 6、
# 缺目标的 4）与只读零写由 tests/cmd_check.rs 覆盖。
#
# 依赖：root、/dev/loop-control、e2fsprogs（mke2fs/e2fsck）、util-linux（losetup/dd）。
source "$(dirname "$0")/lib.sh"
require_fakeonly_bin

img T check_codes.img
# `--align mib` 把首分区放在 LBA 2048（tests/align_spec.rs 同一口径）
START_LBA=2048
# ext4 superblock 位于分区内 1024 字节处；s_log_block_size 在 superblock 内偏移 24。
# 改这一个字节不会动 0x438 处的 ext 魔数，`identify` 仍认作 ext4，e2fsck 打开即失败
SUPER_OFF=$((1024 + 24))

cleanup_hook() { rm -rf "$FAKE_TOOL_DIR"; }

stage() {
    rm -rf "$T" "$T"$SIDECAR_GLOB
    truncate -s 64M "$T" || return 1
    "$B" new "$T" --yes >/dev/null || return 1
    "$B" add "$T" 48M --align mib --yes >/dev/null || return 1
    "$B" mkfs "$T:1" ext4 --yes >/dev/null 2>&1
}

# 恢复记录目录里的条目名：check 不得在证据路径上留下任何东西
sidecar_list() {
    [ -d "$T.diskedit" ] && find "$T.diskedit" -mindepth 1 -printf '%P\n' | sort
    return 0
}

# 镜像上残留的 loop：四码的每条路径都必须在返回前释放自己建的 loop
loops_on_target() {
    losetup -j "$T" -n -O NAME 2>/dev/null | wc -l
}

expect_text() { # haystack needle label
    case "$1" in
        *"$2"*) echo "  OK   $3" ;;
        *) echo "  BAD  $3 (output: $1)"; rc=1 ;;
    esac
}

stage || { echo "无法准备 ext4 夹具: $T" >&2; exit 1; }
BASE=$(mdsum "$T")
SIDE=$(sidecar_list)

echo "== 工具退出 0 -> 0"
OUT=$("$B" check "$T:1" 2>&1)
exp "$?" 0 "clean ext4 passes"
expect_text "$OUT" "check done on partition #1 (ext)" "stdout names the partition and fs"
assert_eq "$(mdsum "$T")" "$BASE" "check did not write the target"
assert_eq "$(sidecar_list)" "$SIDE" "check did not touch the evidence path"
assert_eq "$(loops_on_target)" 0 "check released its loop"

echo "== 工具非零 -> 3"
printf '\xff' | dd of="$T" bs=1 seek=$((START_LBA * 512 + SUPER_OFF)) count=1 conv=notrunc 2>/dev/null
OUT=$("$B" check "$T:1" 2>&1)
exp "$?" 3 "e2fsck errors become a refusal"
expect_text "$OUT" "e2fsck reported errors" "the refusal names the tool's verdict"
assert_eq "$(loops_on_target)" 0 "the refusal released its loop"

echo "== 工具不在 PATH -> 6"
stage || { echo "无法重建 ext4 夹具" >&2; exit 1; }
BASE=$(mdsum "$T")
rm -rf "$FAKE_TOOL_DIR"
mkdir -p "$FAKE_TOOL_DIR"
for t in losetup udevadm; do
    p=$(command -v "$t") || { echo "缺少 $t" >&2; exit 1; }
    ln -sf "$p" "$FAKE_TOOL_DIR/$t"
done
OUT=$("$BFKO" check "$T:1" 2>&1)
exp "$?" 6 "a missing tool is a capability gap"
expect_text "$OUT" "required tool not found" "the gap names the missing table"
expect_text "$OUT" "e2fsck" "the gap names the missing tool"
assert_eq "$(mdsum "$T")" "$BASE" "a capability gap did not write the target"
assert_eq "$(loops_on_target)" 0 "a capability gap released its loop"

echo "== 已找到但启动失败 -> 4"
rm -rf "$FAKE_TOOL_DIR"
mkdir -p "$FAKE_TOOL_DIR"
printf 'not an executable\n' > "$FAKE_TOOL_DIR/e2fsck"
chmod +x "$FAKE_TOOL_DIR/e2fsck"
OUT=$("$BFK" check "$T:1" 2>&1)
exp "$?" 4 "a tool that cannot start is an I/O failure"
expect_text "$OUT" "check failed" "the failure keeps the check context"
assert_eq "$(mdsum "$T")" "$BASE" "a failed start did not write the target"
assert_eq "$(loops_on_target)" 0 "a failed start released its loop"

exit $rc
