# sites-e2fsprogs-resize-fail-goto

## 判断
resize2fs 内部失败清理路径（95 行）。全批 95 行已逐行翻态，note 见同名 tsv。

## 依据
SA2-001（e2fsck→resize2fs→dumpe2fs 次序）、SA2-002（短文件系统落已写待恢复）、SA2-003（回读须填满目标）；src/app/execute/filesystem.rs::grow_plan/shrink/verify_ext_size；tests/online.rs::a_short_filesystem_after_the_tool_is_partial

## 撤回条件
本实现改自研 resize2fs，或 SA2-001/002/003 的“只消费退出码+回读”边界被推翻时重审。
