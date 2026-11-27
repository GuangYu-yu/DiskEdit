# 批次：sites-ntfs-3g-label-uuid-refusal（8 行）

## 判断
ntfslabel 拒绝面：CLI/显式 serial 解析面裁剪（本实现 serial 仅生成）；挂载拒绝认领（占用闸）；超长标签截断登记差异。

## 四态
认领 2 / 拒绝 0 / 差异 1 / 裁剪 3

## 依据
- SA2-009/015（已认领）：ntfsresize 固定参数（扩 -f -f / 缩 -f --size 绝对字节）与卷尺寸回读，真机锚 tests/qemu_suite.rs::s18/s21；
- 消费面：src/app/execute/filesystem.rs::grow_plan/shrink（ntfsresize）、check_arguments（ntfsfix -n）、filesystem.rs label/serial（ntfslabel）、crates/core/src/fs/attrs.rs::label_problem（128 UTF-16）；
- 范围依据：挂载驱动/FUSE、ntfsfix 修复、ntfsinfo/ntfscluster 探针不消费；libntfs-3g 内部错误通道按架构差异拒绝（SA1-021 同族防御面口径）。

## 撤回条件
- 裁剪撤回：档案表登记 ntfsfix 修复面、ntfsinfo/ntfscluster 消费面或显式 serial 设置面时，重新逐行判断；
- 拒绝撤回：出现依赖 libntfs-3g 内部错误通道（ntfs_log）语义的外部契约时重新评估（SA1-021 同族防御面口径）；
- 认领撤回：ntfsresize 固定参数面（扩 -f -f / 缩 -f --size）或 SA2-009/015 行为定义变更时复核对应行；
- 差异撤回：出现依赖"超长截断"语义的外部契约，或要求与基线逐字节等价改标时重新评估。

## 换版重放补充（上游 2022.10.3 → 2026.9.28）

- 本批换版重放新增/改写待判行 5 行，全部翻态：认领 0 / 拒绝 0 / 差异 0 / 裁剪 5。
- ntfslabel parse_options 参数拒绝面维持裁剪——固定形式调用不可达，serial 值由本实现生成后传入（src/app/execute/filesystem.rs）。
- 撤回条件（本节新增行）：
- 裁剪撤回：档案表登记 ntfsinfo/ntfslabel CLI 消费面或 ntfs-3g 新特性面时，重新逐行判断（沿用上文裁剪撤回条件）；
- 拒绝撤回：出现依赖 libntfs-3g 内部错误通道（ntfs_log）语义的外部契约时重新评估（SA1-021 同族防御面口径）；
- 认领撤回：ntfsresize 固定参数面（扩 -f -f / 缩 -f --size）或 SA2-009/015 行为定义变更时复核对应行。
