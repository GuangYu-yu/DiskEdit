#!/usr/bin/env bash
# 在线路径（挂载态）拒绝分支：resizefs --online 的形态守卫 + 非 btrfs 在线缩容拒绝。
# 每条拒绝之后核对「分区表 / FS 未被改动」（info 的 last_lba、dumpe2fs 的 block count×size）。
# 退出码契约唯一定义在 src/outcome.rs:17-25（10=拒绝，本次未写盘）。
# 依据以 `src/文件:行号` 标注在断言旁。
source "$(dirname "$0")/lib.sh"
require_bin

T=/var/tmp/online_guards.img
MP=/testmnt
track_file "$T"

fs_bytes() { # 设备：Block count × Block size
  dumpe2fs -h "$1" 2>/dev/null | awk '/^Block count:/ {c=$3} /^Block size:/ {s=$3} END {print c*s}'
}
lba1() { $B info "$T" 2>/dev/null | grep -o '"last_lba":[0-9]*' | head -1; }

rm -f "$T" "$T$JOURNAL_SUFFIX"
truncate -s 64M "$T"
$B new "$T" --yes >/dev/null 2>&1; exp $? 0 "new（64MiB GPT）"
$B create "$T" --size 16M --name p1 --fs ext4 >/dev/null 2>&1; exp $? 0 "create 16MiB ext4 p1"

LD=$(lo_attach "$T") || { echo "  BAD  losetup 失败"; rc=1; exit 1; }
mount_at "${LD}p1" "$MP" || { echo "  BAD  mount 失败"; rc=1; exit 1; }

# 拒绝前的现场快照：分区表末端 LBA 与 FS 字节数
LAST0=$(lba1)
FS0=$(fs_bytes "${LD}p1")

echo "== 形态守卫：在线形式只接挂载点 =="
# src/cmd/fs.rs:89-91 —— --online 收到 <target>:N 即 refused（10）
$B resizefs "$T":1 --online >/dev/null 2>&1; exp $? 10 "resizefs <target>:1 --online 拒绝"
exp "$(lba1)" "$LAST0" "  拒绝后分区表未变（last_lba 一致）"

# src/cmd/fs.rs:94-96 —— 在线形式不消费 --sector-size，显式拒绝而非静默忽略
$B resizefs "$MP" --online --sector-size 512 >/dev/null 2>&1; exp $? 10 "resizefs <mountpoint> --online --sector-size 拒绝"
exp "$(lba1)" "$LAST0" "  拒绝后分区表未变（last_lba 一致）"

# src/cmd/fs.rs:98-99 —— 目标尺寸 BYTES 与 --size 两种给法必须二选一
$B resizefs "$MP" 8M --online --size 8M >/dev/null 2>&1; exp $? 10 "resizefs <mountpoint> --online BYTES + --size 同给拒绝"
exp "$(lba1)" "$LAST0" "  拒绝后分区表未变（last_lba 一致）"

echo "== 非 btrfs 在线缩容拒绝 =="
# src/online.rs:710-721 —— online_shrink 仅 btrfs；ext4 请求 8MiB < 现分区 16MiB
# 走 Outcome::refused；src/cmd/fs.rs:106-114 将该 Outcome 映射为退出码 10（未写盘）
$B resizefs "$MP" 8M --online >/dev/null 2>&1; exp $? 10 "ext4 在线缩容（8MiB < 16MiB 分区）拒绝"
exp "$(lba1)" "$LAST0" "  拒绝后分区表未变（last_lba 一致）"
exp "$(fs_bytes "${LD}p1")" "$FS0" "  拒绝后 FS 未变（block count×size 一致）"

echo
echo "==== 在线守卫结束（rc=$rc）===="
exit $rc