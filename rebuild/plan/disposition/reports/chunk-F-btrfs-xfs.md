# chunk-F 报告：btrfs-progs / xfsprogs 站点账本处置（32 批，1,836 行）

范围：`plan/disposition/05-sites-btrfs-progs/`（20 批，1,374 行）、`plan/disposition/05-sites-xfsprogs/`（12 批，462 行）。
行原文：`plan/baseline/reference/sites/{btrfs-progs,xfsprogs}.tsv`。全部行已翻态，无"待判"残留；queue.tsv 未动；未改任何代码。
含裁剪行的 22 个批次均已创建同名 `.md`（判断/依据/撤回条件）；纯拒绝批 10 个不建 md（理由在行 note）。

## 判断口径

- **消费面**：btrfs = `btrfs filesystem resize max/<N>k`（在线/离线扩缩）、`btrfs inspect-internal dump-super -f` 回读（SA2-006/007/008/016）、`btrfs check --readonly`、`btrfs filesystem label/show`、`btrfstune -f -U/-u`、`mkfs.btrfs -f [-L]`（catalog.rs 登记，SA 未逐点定案）；xfs = `xfs_growfs <mount>` 与 `-n` 回读（SA2-004/005），check/mkfs/attrs（xfs_repair -n / mkfs.xfs / xfs_admin）同型登记。
- **认领**：resize/growfs 路径的参数校验、回读、错误/退出码路径引 SA2；check 运行前挂载闸与 btrfstune fsid 变更拒绝面按"调用时生效、退出码消费"认领（引代码路径）。
- **拒绝**：上游 C 防御——assert/BUG_ON/abort、失败 goto 清理链、内部不变量致命出口——逐点复刻属多虑（架构差异-多虑），错误经 Rust 类型化传播。
- **裁剪**：未消费子命令/特性（btrfs：df/show/sync/scan/defrag/mkswapfile/usage/property/device 级、tune 的 change-csum/convert-bgt/quota/seeding、btrfstune -m；xfs：xfs_db 全部、mkfs.xfs/proto 选项面、growfs 的 -D/-i/-l/-r/-m 选项段与实验收缩）与工具自身 usage/打印面（SA2-011 同型：固定参数调用，SA 未定案的内部面不逐点认领）。

## 一、待实现工作项

无（0 项）。resize/growfs 消费面的校验、回读与失败语义均已由 SA2-004/005/006/007/008/016 定案并有测试锚（tests/online.rs 四条 btrfs 用例、an_xfs_grow_is_verified_by_the_data_geometry）；本批未发现缺校验/缺回读/提示面缺失的新增面。

观察 2 条（移交主控，不落行态）：
1. `diagnose.rs::run` 把 check 工具的一切非零退出（含"已挂载须 --force"这类运行前拒绝，行 btrfs-progs-00975/00977）都映射为 `Problems`（"check reported problems"），语义上把"拒绝检查"呈现成"发现问题"。建议后续核对是否需要区分"拒绝/问题"两类消息（提示面微调，非行为缺口）。
2. 本实现消费 `btrfs check`/`btrfstune`/`mkfs.btrfs`/`mkfs.xfs` 等能力在 capability-matrix 已登记，但 SA 账本没有对应定案行（SA2-012 枚举不含 btrfs/xfs 的 mkfs/check 覆盖）；本批相应行按"调用时生效→认领（代码路径）"或"SA 未定案→裁剪"处置，建议主控考虑补一条 SA2 行把"btrfs/xfs check+mkfs 的固定参数消费面"入账。

## 二、差异建议

无差异态行（0 条）。与参考的取舍全部归入裁剪（devid/多设备 resize、增量扩缩、-m metadata UUID、growfs 选项段、xfs_db），撤回条件见各批次 md。

## 三、拆分建议（行 note 已标 `建议拆分`，不强拆）

1. `sites-btrfs-progs-frontend-refusal`：12 认领（resize 校验）+ 72 裁剪（未消费子命令）→ 可拆 `sites-btrfs-progs-frontend-resize`。
2. `sites-btrfs-progs-frontend-fail-goto`：8 拒绝（resize goto）+ 60 裁剪 → 同上族。
3. `sites-btrfs-progs-check-refusal`：2 认领（运行前挂载闸）+ 248 裁剪 → 可拆 `sites-btrfs-progs-check-gate`。
4. `sites-btrfs-progs-label-uuid-refusal`：5 认领（fsid 变更）+ 2 裁剪（-m）→ 可拆 `sites-btrfs-progs-label-uuid-fsid`。
5. xfs 三个 resize 批（error-report/fatal-exit/refusal）各含认领核心行与选项段裁剪行 → 可合并拆出 `sites-xfsprogs-resize-growfs-core`（15 认领行）。

## 四、四态计数

| 批次 | 认领 | 拒绝 | 差异 | 裁剪 | 小计 |
|---|---|---|---|---|---|
| sites-btrfs-progs-check-abort | 0 | 3 | 0 | 0 | 3 |
| sites-btrfs-progs-check-assert | 0 | 49 | 0 | 0 | 49 |
| sites-btrfs-progs-check-error-report | 0 | 0 | 0 | 132 | 132 |
| sites-btrfs-progs-check-fail-goto | 0 | 332 | 0 | 0 | 332 |
| sites-btrfs-progs-check-fatal-exit | 0 | 0 | 0 | 12 | 12 |
| sites-btrfs-progs-check-refusal | 2 | 0 | 0 | 248 | 250 |
| sites-btrfs-progs-check-string-channel | 0 | 0 | 0 | 3 | 3 |
| sites-btrfs-progs-frontend-fail-goto | 0 | 8 | 0 | 60 | 68 |
| sites-btrfs-progs-frontend-fatal-exit | 0 | 0 | 0 | 5 | 5 |
| sites-btrfs-progs-frontend-refusal | 12 | 0 | 0 | 72 | 84 |
| sites-btrfs-progs-frontend-string-channel | 0 | 0 | 0 | 1 | 1 |
| sites-btrfs-progs-label-uuid-fail-goto | 0 | 13 | 0 | 0 | 13 |
| sites-btrfs-progs-label-uuid-refusal | 5 | 0 | 0 | 2 | 7 |
| sites-btrfs-progs-misc-assert | 0 | 9 | 0 | 0 | 9 |
| sites-btrfs-progs-misc-fail-goto | 0 | 55 | 0 | 0 | 55 |
| sites-btrfs-progs-misc-fatal-exit | 0 | 0 | 0 | 3 | 3 |
| sites-btrfs-progs-misc-refusal | 0 | 0 | 0 | 89 | 89 |
| sites-btrfs-progs-mkfs-fail-goto | 0 | 132 | 0 | 0 | 132 |
| sites-btrfs-progs-mkfs-fatal-exit | 0 | 0 | 0 | 14 | 14 |
| sites-btrfs-progs-mkfs-refusal | 0 | 0 | 0 | 113 | 113 |
| sites-xfsprogs-misc-assert | 0 | 1 | 0 | 0 | 1 |
| sites-xfsprogs-misc-error-report | 0 | 0 | 0 | 3 | 3 |
| sites-xfsprogs-misc-fatal-exit | 0 | 0 | 0 | 8 | 8 |
| sites-xfsprogs-misc-refusal | 0 | 0 | 0 | 25 | 25 |
| sites-xfsprogs-mkfs-assert | 0 | 6 | 0 | 0 | 6 |
| sites-xfsprogs-mkfs-error-report | 0 | 0 | 0 | 65 | 65 |
| sites-xfsprogs-mkfs-fail-goto | 0 | 7 | 0 | 0 | 7 |
| sites-xfsprogs-mkfs-fatal-exit | 0 | 0 | 0 | 125 | 125 |
| sites-xfsprogs-mkfs-refusal | 0 | 0 | 0 | 177 | 177 |
| sites-xfsprogs-resize-error-report | 4 | 0 | 0 | 6 | 10 |
| sites-xfsprogs-resize-fatal-exit | 4 | 0 | 0 | 9 | 13 |
| sites-xfsprogs-resize-refusal | 7 | 0 | 0 | 15 | 22 |
| **btrfs-progs 小计（20 批）** | **19** | **601** | **0** | **754** | **1,374** |
| **xfsprogs 小计（12 批）** | **15** | **14** | **0** | **433** | **462** |
| **合计（32 批）** | **34** | **615** | **0** | **1,187** | **1,836** |

判定依据主干：SA2-004/005（xfs_growfs 参数与 -n 几何回读）、SA2-006/007/008/016（btrfs resize 与 superblock 回读）、SA1-005（覆写闸）、B17（多设备占用证据）；消费面登记 crates/core/src/fs/catalog.rs、参数固定 src/platform/linux/tools.rs、退出码/回读 src/app/execute/filesystem.rs 与 diagnose.rs。
