# 05-sites-libfdisk/sites-libfdisk-table-edit-refusal-libfdisk（换版重放批次，32 行）

判断沿承行 note 与同名旧批判断（sources.tsv 2026-11-27 换版重放）；逐行 state 已迁移，本批为归类重建。

## 撤回条件

沿承行 note 引用的 SA/D 条目撤回条件；本批仅归类调整，不改变判断。

## 换版重放补充（2026-11-27：libfdisk/util-linux 2.39.3→2.42.4）

- 本批新增/改写 1 行（00131 bsd.c “Too many partitions”），判**裁剪**：老盘标（bsd/sun/sgi/amiga/mac/dvh）不识别，统一按未知拒绝（SA1-016）。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：SA1-016 重开（出现必须识别这些盘标的外部契约）时重审。
