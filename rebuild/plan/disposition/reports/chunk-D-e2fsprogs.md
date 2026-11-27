# chunk-D：e2fsprogs 站点账本处置报告

范围：`plan/disposition/05-sites-e2fsprogs/` 全部 30 个批次 tsv，共 2121 行（行原文：`plan/baseline/reference/sites/e2fsprogs.tsv`）。

## 四态计数

| 认领 | 拒绝 | 差异 | 裁剪 | 合计 |
| --- | --- | --- | --- | --- |
| 12 | 6 | 2 | 2101 | 2121 |

## 总体判断

e2fsprogs 站点几乎全部是外部工具（e2fsck/resize2fs/mke2fs/tune2fs/dumpe2fs/libext2fs）源码内部的防御、清理、文案与 problem 表。
本实现不实现这些工具：ext 的 check/resize 走档案表固定程序序列（e2fsck -f -n 只读检查；离线扩缩 e2fsck -fp→resize2fs→dumpe2fs，SA2-001/002/003），attrs 面只有卷标与 UUID（tune2fs -L/-U/-l 固定形式），mkfs 面只有类型与卷标（mke2fs -t extN [-L label]）。
因此默认处置为裁剪（绑定“本实现不实现 fs 修复/工具内部路径”的特性依据）；退出码产出端与已有参数校验认领；两处“截断 vs 拒绝”登记差异建议。

## 待实现工作项

无（0 项）。理由：
- 本实现已暴露参数的校验齐全：ext 卷标 16 字节 s_volume_name 容量（`crates/core/src/fs/attrs.rs::label_problem`，mkfs 在擦除前前置校验 `src/app/decide/mkfs.rs`、set 在判定层校验 `src/app/decide/edit.rs::content_property`）、UUID 形状（`src/app/decide/edit.rs::is_uuid`）、占用闸（`src/app/decide/edit.rs::precheck`）——对应 tune2fs/mke2fs 的 label/UUID 行均已认领或登记差异；
- 其余行为绑定外部工具内部路径，只消费退出码（SA2-001/002），按裁剪处置，不产生工作项。

## 差异建议（交主控统一登记 D 编号）

1. `e2fsprogs-00317`（tune2fs_main，label-uuid-refusal）：set label/ext 卷标超长时本实现前置拒绝（exit 3），参考工具截断+警告后照常设置。`D-建议:ext 卷标超长按 16 字节容量前置拒绝，不采纳截断`——依据 `crates/core/src/fs/attrs.rs::label_problem`、`src/app/decide/edit.rs::content_property`、`tests/cases/mod.rs::set_label_on_ext_refuses_an_over-long_label`。撤回条件：出现依赖“超长截断”语义的外部契约或要求与基线逐字节等价改标。
2. `e2fsprogs-00481`（mke2fs PRS，mkfs-refusal）：mkfs/ext 卷标超长时本实现在擦除任何字节前拒绝，参考工具截断后建卷。`D-建议:mkfs 卷标超长在写任何字节前按容量拒绝，不采纳截断建卷`——依据 `src/app/decide/mkfs.rs` 的 label_problem 前置闸。撤回条件同上。

## 观察（移交主控，不属本批 ledger 行）

- ext 收缩无 `resize2fs -P` 最小尺寸前置闸：refusal-map R0211/R0382/R0416（旧版“target size < estimated minimum FS size (resize2fs -P)”拒绝点）的认领指向 `src/app/decide/resize.rs`，但新实现中 `Refusal::BelowMinimumSize`/`MinimumSizeUnknown` 无任何构造点，最小尺寸核查交由 resize2fs 自身拒绝（非零退出落已写待恢复，SA2-002）+ dumpe2fs 回读（SA2-003）兜底；ext 离线收缩“先缩文件系统后缩表”的次序保证盘面安全，但“写前明确拒绝”的旧版对外语义是否仍被认领成立，建议主控核对 refusal-map 该三行，或登记差异/新工作项（标签 缺校验，建议硬约束：用例断言“ext 收缩小于 resize2fs -P 最小值时在写表前拒绝或显式登记差异”）。本批对应 ledger 行 e2fsprogs-00102/00103（calculate_minimum_resize_size 的 #if DEBUG 打印）已按裁剪处置。

## 拆分建议

- 批次已按 area×category 切分，无需强拆。两批出现混合结论，仅在行 note 标注 `建议拆分`（不强拆）：
  - `sites-e2fsprogs-check-fatal-exit`：7 行退出码产出端认领 → 建议新批 `sites-e2fsprogs-check-exit-contract`；
  - `sites-e2fsprogs-label-uuid-refusal`：5 认领 + 6 拒绝 + 1 差异 → 建议新批 `sites-e2fsprogs-label-uuid-set-surface`。

## 逐批明细

| 批次 | 行数 | 结论 |
| --- | --- | --- |
| sites-e2fsprogs-check-abort | 1 | e2fsck sigcatcher 的 abort() |
| sites-e2fsprogs-check-error-report | 486 | e2fsck 全部 com_err/printf 文案站点 |
| sites-e2fsprogs-check-fail-goto | 130 | e2fsck 内部失败清理路径 |
| sites-e2fsprogs-check-fatal-exit | 54 | e2fsck 出口站点 |
| sites-e2fsprogs-check-message-entry | 311 | e2fsck problem 表 311 行 |
| sites-e2fsprogs-check-refusal | 271 | e2fsck 内部拒绝、problem 表字符串与 fix_problem 站点 |
| sites-e2fsprogs-check-string-channel | 7 | e2fsck DEBUG/#ifdef 打印与内部 puts |
| sites-e2fsprogs-label-uuid-error-report | 60 | tune2fs/e2label 内部 com_err/printf 文案 |
| sites-e2fsprogs-label-uuid-fail-goto | 32 | tune2fs 内部失败清理 |
| sites-e2fsprogs-label-uuid-fatal-exit | 33 | tune2fs/e2label usage 与退出出口 |
| sites-e2fsprogs-label-uuid-refusal | 103 | tune2fs 参数面与内部拒绝 |
| sites-e2fsprogs-label-uuid-string-channel | 1 | tune2fs metadata_csum 校验重写失败打印 |
| sites-e2fsprogs-misc-assert | 1 | lib/support/dict.c 内部断言 |
| sites-e2fsprogs-misc-error-report | 27 | libext2fs/libsupport 内部文案 |
| sites-e2fsprogs-misc-fail-goto | 85 | ext2fs_open2/ext2fs_flush2/quota/profile 库内部失败清理 |
| sites-e2fsprogs-misc-fatal-exit | 8 | 库辅助/测试程序出口 |
| sites-e2fsprogs-misc-refusal | 3 | 库/辅助工具内部输出与拒绝 |
| sites-e2fsprogs-misc-string-channel | 6 | dict/profile 内部 puts |
| sites-e2fsprogs-mkfs-error-report | 85 | mke2fs 建卷过程消息 |
| sites-e2fsprogs-mkfs-fail-goto | 28 | mke2fs 内部失败清理 |
| sites-e2fsprogs-mkfs-fatal-exit | 116 | mke2fs usage/OOM 出口 |
| sites-e2fsprogs-mkfs-refusal | 124 | mke2fs CLI 参数与内部校验 |
| sites-e2fsprogs-probe-error-report | 18 | dumpe2fs 全量输出与内部消息 |
| sites-e2fsprogs-probe-fail-goto | 3 | dumpe2fs 内部失败清理 |
| sites-e2fsprogs-probe-fatal-exit | 13 | dumpe2fs usage 与出口 |
| sites-e2fsprogs-probe-refusal | 12 | dumpe2fs MMP/journal/badblocks 输出面拒绝与 -o 参数校验 |
| sites-e2fsprogs-resize-fail-goto | 95 | resize2fs 内部失败清理路径 |
| sites-e2fsprogs-resize-fatal-exit | 3 | resize2fs exit(1) 出口 |
| sites-e2fsprogs-resize-refusal | 3 | resize2fs 自身拒绝 |
| sites-e2fsprogs-resize-string-channel | 2 | calculate_minimum_resize_size 的 #if DEBUG 打印 |

每批含裁剪/差异行，均已创建同名 md（判断/依据/撤回条件）。