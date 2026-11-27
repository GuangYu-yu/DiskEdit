# sites-e2fsprogs-probe-fatal-exit

## 判断
dumpe2fs usage 与出口（14 行）。全批 13 行已逐行翻态，note 见同名 tsv。

## 依据
固定参数 -h 调用，usage 不可达；SA2-003

## 撤回条件
出现依赖 usage 码的外部契约时重审。

## 换版重放补充（2026-11-27：e2fsprogs 1.47.0→1.47.4）

- 本批新增/改写 2 行（00729 usage、00731 flag 组合出口），全部判**裁剪**：dumpe2fs 以固定参数（-h）调用，usage 出口不可达；非零码由回读闸兜住（SA2-003）。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：探针面扩大时重审。
