# sites-e2fsprogs-probe-error-report

## 判断
dumpe2fs 全量输出与内部消息（19 行）。全批 18 行已逐行翻态，note 见同名 tsv。

## 依据
SA2-003/SA2-011（只解析 Block count/Block size 稳定字段，缺字段即拒绝）；SA2-002

## 撤回条件
回读字段解析变化（版本漂移，SA2-011 撤回条件）或引入全量输出依赖时重审。

## 换版重放补充（2026-11-27：e2fsprogs 1.47.0→1.47.4）

- 本批新增/改写 1 行（00730 dumpe2fs 设备名解析失败消息），判**裁剪**：本实现以固定 -h 参数调用且设备名由自身解析层处理（R0046 族）；输出只消费 Block count/Block size 稳定字段（SA2-003/SA2-011）。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：探针面扩大（消费更多 dumpe2fs 字段）或自研探针时重审。
