# 换版重放处置报告：btrfs-progs 6.6.3→7.3 / xfsprogs 6.6.0→7.3.0

范围：`plan/disposition/05-sites-btrfs-progs/`、`plan/disposition/05-sites-xfsprogs/` 内 state=待判 的行（上游换版新增/改写行），共 **641 行**，全部翻态完毕。只改批次 tsv 与同名 md（末尾追加"换版重放补充"小节）、新建 1 个缺失 md；queue.tsv 未触碰。行原文取自 `plan/baseline/reference/sites/btrfs-progs.tsv` / `xfsprogs.tsv`（7.3 / 7.3.0，见 sources.tsv）。

## 四态计数

| 工具 | 认领 | 拒绝 | 差异 | 裁剪 | 合计 |
| --- | --- | --- | --- | --- | --- |
| btrfs-progs（355 行 / 14 批） | 3 | 104 | 0 | 248 | 355 |
| xfsprogs（286 行 / 9 批） | 0 | 24 | 0 | 262 | 286 |
| **合计** | **3** | **128** | **0** | **510** | **641** |

分批计数（认领/拒绝/差异/裁剪）：

- btrfs：check-assert 0/7/0/0；check-error-report 0/0/0/25；check-fail-goto 0/16/0/0；check-fatal-exit 0/0/0/2；check-refusal 0/0/0/37；frontend-fail-goto 0/4/0/17；frontend-refusal 3/0/0/33；misc-assert 0/3/0/0；misc-fail-goto 0/13/0/0；misc-refusal 0/0/0/9；mkfs-assert 0/1/0/0；mkfs-fail-goto 0/60/0/0；mkfs-fatal-exit 0/0/0/4；mkfs-refusal 0/0/0/121。
- xfs：misc-error-report 0/0/0/1；misc-refusal 0/0/0/1；mkfs-assert 0/1/0/0；mkfs-error-report 0/0/0/31；mkfs-fail-goto 0/23/0/0；mkfs-fatal-exit 0/0/0/77；mkfs-refusal 0/0/0/150；mkfs-string-channel 0/0/0/1；resize-fatal-exit 0/0/0/1。

## 待实现工作项

**0 条。** 本批 3 个认领行全部落在既有消费面，均有现成代码路径承载：

- btrfs-progs-00033/00035（resize 尺寸串校验/解析）→ SA2-016 + `src/app/execute/filesystem.rs::btrfs_size/grow_plan/shrink`（自建 `<N>k` 尺寸串）；
- btrfs-progs-00061（resize 调用时独占操作检查错误路径）→ SA2-006/007 + `src/app/execute/filesystem.rs::online_invocation`。

其余新增/改写行：或绑定固定参数调用不可达/未消费特性（裁剪），或属上游 C 防御模式（拒绝，架构差异-多虑），不构成"应有而未实现"项。

## 新增差异建议（D-建议）

无。换版未产生与本实现消费面冲突的行为分叉：本实现固定调用形态（btrfs `resize max` / `resize <N>k`、`btrfs check --readonly`、`mkfs.btrfs -f [-L]`、`mkfs.xfs -f [-L]`、`xfs_growfs <mount>` / `xfs_growfs -n`）在新版语义不变，无需主控登记 D 项。

## 值得注意的上游行为变化摘要

1. **btrfs resize 新增 `--offline` / `--enqueue` 与镜像文件 resize**（cmds/filesystem.c check_offline_resize_args/offline_resize）：离线扩容、排队执行、对文件镜像扩容均为新形态。本实现按 SA2-016 走挂载后在线 resize，相关 15 行（00036-00046/00058/00060）裁剪；撤回条件见 frontend-refusal md 补充小节。
2. **btrfs resize 仍为单设备隐式 devid 1（缺失时回落 mindev）**：本实现只发 `max` 与绝对目标 `<N>k`，devid 前缀（00034/00052）与按增量扩缩 `-X/+X` 形态（00054/00056）不在消费面。
3. **btrfs check 新增 `--skip-qgroup-accounting` / `--qgroup-report` 选项、[1/8]–[8/8] 阶段进度打印、--repair 专属 replace/balance 闸与 repair.c dev extent 修复路径**：固定 `check --readonly` 下均不可达，裁剪（check-fatal-exit/check-refusal）。
4. **btrfs tune 新增 change-csum（换校验算法）与 convert-remap-tree（remap-tree 新 incompat 特性）**：btrfstune 未消费特性面，裁剪/拒绝按既有批次口径。
5. **btrfs frontend 新增 `filesystem mkswapfile`、`filesystem commit-stats` 子命令与 device stats `--offline`/`--reset` 选项**：未消费子命令面，裁剪。
6. **xfsprogs mkfs.xfs 7.3.0 选项约束链大幅扩展**：metadir/exchrange/parent pointers/autofsck/persistent quota 组合约束、zoned RT 与 rgsize/rgcount（realtime 分组）、max_atomic_write 上限链、`-c defaults` 配置文件、proto 文件面改写为 metadir/rtgroup inode 路径——固定 `mkfs.xfs -f [-L]` 下不可达或随工具自带，裁剪/拒绝按批次口径。
7. **xfs_growfs 选项集扩展（-x/-R/-t 等）但无参 grow 与 `-n` 报告模式语义未变**：SA2-004/005 消费边界不受影响；本轮唯一换版行 00019（-r realtime 段出口）裁剪。

## 产出文件

- 批次 tsv 翻态：05-sites-btrfs-progs/ 14 个 tsv、05-sites-xfsprogs/ 9 个 tsv（全部无残留待判）。
- md 追加"换版重放补充"小节（含撤回条件）：btrfs 8 个（check-error-report / check-fatal-exit / check-refusal / frontend-fail-goto / frontend-refusal / misc-refusal / mkfs-fatal-exit / mkfs-refusal）、xfs 6 个（misc-error-report / misc-refusal / mkfs-error-report / mkfs-fatal-exit / mkfs-refusal / resize-fatal-exit）。
- md 新建：05-sites-xfsprogs/sites-xfsprogs-mkfs-string-channel.md（该批首现裁剪行，按协议补建撤回条件文档）。
- 校验：各 tsv 内"同上"链全量解析无悬挂、无占位 note 残留；queue.tsv 与代码目录未改动。
