# sites-e2fsprogs-label-uuid-refusal

## 判断
tune2fs 参数面与内部拒绝（102 行）：5 行认领（00324/00307/00308/00325/00326——挂载拒绝同向、UUID 形状前置校验）、6 行拒绝（00288-00293 在线改标路径不采纳）、1 行差异（00317 截断 vs 拒绝）、91 行裁剪。全批 103 行已逐行翻态，note 见同名 tsv。

## 依据
SA2-001 同族占用闸（B4/B17）；src/app/decide/edit.rs::precheck 与 ::is_uuid；crates/core/src/fs/attrs.rs::label_problem；tests/cases/mod.rs::set_label_on_ext_refuses_an_over-long_label

## 撤回条件
认领行：占用闸语义或 UUID 校验变化时重审；拒绝行：出现必须在挂载卷上在线改标的外部契约且占用闸允许放行时重审；差异行（00317）：确认存在依赖“超长卷标截断”语义的外部契约、或要求与基线工具逐字节等价改标时，撤回拒绝改为工具同语义；裁剪行：扩大 attrs 参数面时重审。

## 换版重放补充（2026-11-27：e2fsprogs 1.47.0→1.47.4）

- 本批新增/改写 2 行（00319 quota 特性开关、00325 inode 尺寸的“卸载才可改”校验），全部判**裁剪**：参数面不接线（本实现 attrs 面只有卷标与 UUID，tune2fs -L/-U/-l 固定形式）；挂载中目标由占用闸更早拒绝（B4/B17）。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：attrs 面扩大（接 quota/inode-size 参数）时，按本实现占用闸重实现该校验并重审。
