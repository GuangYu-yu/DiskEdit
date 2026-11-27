# sites-e2fsprogs-mkfs-fail-goto

## 判断
mke2fs 内部失败清理（29 行）。全批 28 行已逐行翻态，note 见同名 tsv。

## 依据
同上

## 撤回条件
同上。

## 换版重放补充（2026-11-27：e2fsprogs 1.47.0→1.47.4）

- 本批新增/改写 1 行（02027 lib/ext2fs initialize 内部清理），判**裁剪**：随外部工具自带，mke2fs 固定参数调用、非零退出即失败。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：扩大 mkfs 参数面或自研 mke2fs 时重审。
