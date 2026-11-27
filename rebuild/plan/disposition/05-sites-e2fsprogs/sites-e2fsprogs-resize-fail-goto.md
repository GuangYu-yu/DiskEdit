# sites-e2fsprogs-resize-fail-goto

## 判断
resize2fs 内部失败清理路径（96 行）。全批 95 行已逐行翻态，note 见同名 tsv。

## 依据
SA2-001（e2fsck→resize2fs→dumpe2fs 次序）、SA2-002（短文件系统落已写待恢复）、SA2-003（回读须填满目标）；src/app/execute/filesystem.rs::grow_plan/shrink/verify_ext_size；tests/online.rs::a_short_filesystem_after_the_tool_is_partial

## 撤回条件
本实现改自研 resize2fs，或 SA2-001/002/003 的“只消费退出码+回读”边界被推翻时重审。

## 换版重放补充（2026-11-27：e2fsprogs 1.47.0→1.47.4）

- 本批新增/改写 2 行（00016/00029 resize2fs 内部清理），全部判**裁剪**：本实现不实现 resize2fs，按固定次序消费退出码并以 dumpe2fs 回读证实（SA2-001 次序、SA2-002 短文件系统落已写待恢复、SA2-003 回读只收填满目标）。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：自研 resize2fs 或退出码/回读消费边界变化时重审。
