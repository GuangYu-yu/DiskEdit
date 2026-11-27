# 批次：sites-ntfs-3g-frontend-refusal（54 行）

## 判断
ntfsfix/ntfscluster 拒绝面：裁剪。

## 四态
认领 0 / 拒绝 0 / 差异 0 / 裁剪 54

## 依据
- SA2-009/015（已认领）：ntfsresize 固定参数（扩 -f -f / 缩 -f --size 绝对字节）与卷尺寸回读，真机锚 tests/qemu_suite.rs::s18/s21；
- 消费面：src/app/execute/filesystem.rs::grow_plan/shrink（ntfsresize）、check_arguments（ntfsfix -n）、filesystem.rs label/serial（ntfslabel）、crates/core/src/fs/attrs.rs::label_problem（128 UTF-16）；
- 范围依据：挂载驱动/FUSE、ntfsfix 修复、ntfsinfo/ntfscluster 探针不消费；libntfs-3g 内部错误通道按架构差异拒绝（SA1-021 同族防御面口径）。

## 撤回条件
- 裁剪撤回：档案表登记 ntfsfix 修复面、ntfsinfo/ntfscluster 消费面或显式 serial 设置面时，重新逐行判断；
- 拒绝撤回：出现依赖 libntfs-3g 内部错误通道（ntfs_log）语义的外部契约时重新评估（SA1-021 同族防御面口径）；
- 认领撤回：ntfsresize 固定参数面（扩 -f -f / 缩 -f --size）或 SA2-009/015 行为定义变更时复核对应行；
- 差异撤回：出现依赖"超长截断"语义的外部契约，或要求与基线逐字节等价改标时重新评估。
