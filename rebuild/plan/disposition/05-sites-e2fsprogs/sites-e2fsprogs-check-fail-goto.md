# sites-e2fsprogs-check-fail-goto

## 判断
e2fsck 内部失败清理路径（131 行）。全批 130 行已逐行翻态，note 见同名 tsv。

## 依据
SA2-001/002；src/app/execute/filesystem.rs::run_checked

## 撤回条件
同上：自研检查器或退出码消费边界变化时重审。

## 换版重放补充（2026-11-27：e2fsprogs 1.47.0→1.47.4）

- 本批新增/改写 4 行（00802/00886/01762/02007，extents/journal/rehash/util 内部清理），全部判**裁剪**：e2fsck 内部失败清理路径随外部工具自带（SA2-001/002）。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：自研检查器或退出码消费边界变化时重审。
