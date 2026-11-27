# 换版重放处置报告：e2fsprogs / libfdisk / parted（05-sites 三目录）

- 重放版本（plan/baseline/reference/sources.tsv，2026-11-27）：e2fsprogs 1.47.0→1.47.4、libfdisk/util-linux 2.39.3→2.42.4、parted 3.6→3.8。
- 范围：plan/disposition/05-sites-{e2fsprogs,libfdisk,parted}/ 下 28 个批次 tsv 中 state=待判 的行，共 **144 行**（e2fsprogs 65、libfdisk 58、parted 21）。旧判断已迁移，本报告只覆盖上游新增/改写行；queue.tsv 未改动（由主控统一重算）。
- 逐行结论见各批次 tsv note；含新裁剪行的 27 个批次 md 已在末尾追加「换版重放补充」小节（含撤回条件），parted/sites-parted-frontend-error-report.md 原缺失已按沿承判断新建。
- 上游源码对照：/tmp/refsrc/src（新版本）与 /tmp/src（旧版本）逐行核对，确认哪些站点是真新增、哪些是行号漂移的改写。

## 1. 四态计数

| 工具 | 认领 | 拒绝 | 差异 | 裁剪 | 合计 |
| --- | ---: | ---: | ---: | ---: | ---: |
| e2fsprogs（65） | 0 | 0 | 0 | 65 | 65 |
| libfdisk（58） | 16 | 4 | 0 | 38 | 58 |
| parted（21） | 0 | 3 | 0 | 18 | 21 |
| **合计** | **16** | **7** | **0** | **121** | **144** |

分批计数：

| 批次 | 认领 | 拒绝 | 裁剪 |
| --- | ---: | ---: | ---: |
| e2fsprogs：check-error-report(9)/check-fail-goto(4)/check-message-entry(5)/check-refusal(3)/label-uuid-refusal(2)/misc-refusal(1)/mkfs-error-report(13)/mkfs-fail-goto(1)/mkfs-fatal-exit(17)/mkfs-refusal(5)/probe-error-report(1)/probe-fatal-exit(2)/resize-fail-goto(2) | – | – | 全部 |
| libfdisk：table-edit-fail-goto 4+15、table-edit-fatal-exit 5+2+7、table-edit-refusal-disk-utils 4+10、table-edit-error-report 3+4、table-edit-assert-disk-utils 1 裁剪、table-edit-refusal-libfdisk 1 裁剪 | 16 | 2（--reorder） | 38 |
| libfdisk：mbr-assert(1)、label-uuid-assert(1) | – | 2 | – |
| parted：frontend-assert-libparted 6 裁剪+1 拒绝、table-edit-fail-goto 2 拒绝、其余 7 批 13 行全裁剪 | – | 3 | 18 |

## 2. 待实现工作项

**0 项。** 判定逻辑：本批 144 行中没有一条落在「本实现已接线但缺校验」的面上——

- e2fsprogs 新校验全部绑定 e2fsck 修复器本体（problem 表/fix_problem/ehandler）或未接线的 mke2fs/dumpe2fs 参数面（revision/特性组合/xattr selinux/huge files/块大小），按既有口径判裁剪（SA2-001/002 只消费退出码；mkfs 面只有类型与卷标）；
- libfdisk 的 resize 上界计算（fdisk_partition_get_max_size）本实现已有（resize.rs grow ceiling + edit.rs gaps/first_fit），撞签/擦除面已有（SA1-005/006）、delete/dump/--sector-size 校验已有（parse.rs/R0092），其余为交互菜单、discard 面、备份文件面、条目重排等既有范围裁剪；
- parted 新增/改写站点全部落在 fs 操作面（fs/r/fat、fs/r/hfs、fs/amiga）、老盘标（atari/dvh/fdasd）、C 库内部不变量与异常应答面上，均已被 SA1-016/017/021、SA2-014 覆盖。

待实现项格式（若有）：`row_id | 行为 | 标签 | 根因 | 建议硬约束`——本批无需登记。

## 3. 差异建议（编号由主控统一登记）

1. **D-建议：--sector-size 校验口径。** 上游 fdisk/cfdisk/sfdisk 白名单 512/1024/2048/4096，本实现 parse.rs 按「2 的大幂」校验（R0092），8192/16384 会被本实现接受而上游拒绝。本批 4 行（libfdisk-00955/00956/01128/01129）判认领引 R0092，与已迁移行 libfdisk-01020 同口径；若主控决定收紧为白名单，请一并重审 01020 与 R0092。其余无新增差异行。

## 4. 上游行为变化摘要（按工具）

### e2fsprogs 1.47.0→1.47.4
- e2fsck：新增 ea_inode 族 problem 表条目与 fix_problem 站点（01547/01548/01647/01689/01720 及 pass1/pass4 对应修复判读）、zero dtime、超前 mount/mkfs 时间的 super.c 修复站点；1.47.3 修复 `e2fsck -n` 遇未引用 EA inode 时中断。全部属修复器本体 → 裁剪（SA2-001/002）。
- mke2fs：新增 root inode security.selinux xattr 写入链、`-d` 复制 fs-verity 元数据、RAID stripe/stride 按转动性取值等站点；revision/fs-verity 特性校验措辞调整。参数面未接线 → 裁剪。
- dumpe2fs/resize2fs/lib/ext2fs/plausible：出口与文案漂移，无行为面变化 → 裁剪。

### libfdisk/util-linux 2.39.3→2.42.4
- **新增 `sfdisk --discard-free` 命令**（收集未分区空间并发 BLKDISCARD）：绑定 SA1-006（从不发送 discard/TRIM）→ 裁剪；撤回条件为带二次确认与不可恢复提示的 discard 面重开。
- **fdisk 菜单新增交互 resize 对话框（resize_partition）与 discard 对话框（discard_sectors/discard_partition/discard_freespace）**：交互面裁剪；resize 能力本身已实现（SA1-014）。
- **新增 libfdisk API `fdisk_partition_get_max_size`**（按自由空间求 resize 上界）：本实现已有同能力 → 认领（SA1-008/014）。
- 新增 append 模式撞签提示（"it will not be wiped unless you create a new partition table"）：撞签检测与拒绝已实现 → 认领（SA1-005/006）。
- `--reorder`（fix partitions order）站点：条目重排不提供（SA3-001 同原则）→ 拒绝；`--backup-pt-sectors` 备份文件面 → 裁剪（SA1-020）。

### parted 3.6→3.8
- FAT resize 不可达尺寸的拒绝文案改写（"We're working on it!" → "…too small"）：仍是 FAT 操作面 → 裁剪（SA2-014）。
- 新增 PED_ASSERT：ped_malloc 参数不变量（libparted.c）、hfs/cache 与 fdasd 参数断言；`--fix` 应答消息改走 stderr。前者按架构差异拒绝（SA1-021）/老盘标裁剪（SA1-016），后者绑交互异常应答面 → 裁剪。
- fs/r/fat、fs/r/hfs、fs/amiga、atari/dvh/fdasd 站点均为既有裁剪面内的漂移或新增内部站点，无 GPT/MBR 盘面行为变化；本批未发现需要新增盘面校验的差异。

## 5. 拆分建议

- 无需拆分：各批次内新行结论与既有批次主结论一致（e2fsprogs 全裁剪；libfdisk/parted 裁剪为主、少量认领/拒绝与既有口径同源）。
- libfdisk table-edit-* 三批（fail-goto/fatal-exit/refusal-disk-utils）混合了 2.42.4 新增 discard 面与既有迁移行；若主控希望按「上游新增 discard 面」单独建账，可拆 `sites-libfdisk-discard-face` 批（涉及行：00899–00905、00991–01004、00903），当前不强制。
