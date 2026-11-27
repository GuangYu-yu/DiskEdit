# sites-e2fsprogs-mkfs-error-report

## 判断
mke2fs 建卷过程消息（95 行）。全批 85 行已逐行翻态，note 见同名 tsv。

## 依据
src/app/execute/filesystem.rs::run_checked；mkfs_invocation 固定参数

## 撤回条件
本实现扩大 mkfs 参数面（块大小/inode 比/特性等）或自研 mke2fs 时重审。

## 换版重放补充（2026-11-27：e2fsprogs 1.47.0→1.47.4）

- 本批新增/改写 13 行（00360/00362/00370/00372/00374/00376/00428/00434/00463/00512/00514/00591/00686，inode 表写入/xattr selinux root 标签/revision 校验/已移除选项/huge files 消息），全部判**裁剪**：mke2fs 建卷过程文案不逐字复刻，工具非零退出即失败（run_checked）；对应参数面不接线（mkfs_invocation 固定参数）。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：扩大 mkfs 参数面（块大小/inode 比/特性/selinux 等）或自研 mke2fs 时重审。
