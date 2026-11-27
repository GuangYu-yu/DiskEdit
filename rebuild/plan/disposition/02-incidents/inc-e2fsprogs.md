# 02-incidents/inc-e2fsprogs（275 行）

## 判断
- 裁剪 274：全部为依赖工具 e2fsprogs（e2fsck/resize2fs/debugfs/blkid/tune2fs/badblocks/mke2fs/logsave）内部修复/特性；本实现只按档案表以 e2fsck/resize2fs/dumpe2fs 承接扩缩与检查（SA2-001/002/003），工具内部随发行版版本固定（SA2-011/012）。
- 认领 1（00224）：e2fsck"仅修复时返回 1、仅优化返回 0"是 SA2-001 检查器退出码契约的基础。

## 依据
- 参考行原文（e2fsprogs RELEASE-NOTES）均为上游程序内部行为；本实现防线在调用面（退出码+回读证实+失败落已写待恢复），不在复刻工具内部。

## 撤回条件
- 若把 e2fsprogs 任一内部能力（如 resize2fs 最小尺寸估算）纳入本实现行为面，重开对应行。
- 若依赖工具版本面变更（SA2-011），复查 00116/00117/00252 等正确性/CVE 行是否仍由上游承担。
