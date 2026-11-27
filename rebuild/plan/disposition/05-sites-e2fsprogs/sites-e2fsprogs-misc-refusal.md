# sites-e2fsprogs-misc-refusal

## 判断
库/辅助工具内部输出与拒绝（3 行）。全批 3 行已逐行翻态，note 见同名 tsv。

## 依据
随外部工具自带

## 撤回条件
同上。

## 换版重放补充（2026-11-27：e2fsprogs 1.47.0→1.47.4）

- 本批新增/改写 1 行（02108 plausible.c “last mounted on/created on” 打印），判**裁剪**：库/辅助工具内部输出随外部工具自带，不接线（同 02107）。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：自研 ext 识别/提示面时重审。
