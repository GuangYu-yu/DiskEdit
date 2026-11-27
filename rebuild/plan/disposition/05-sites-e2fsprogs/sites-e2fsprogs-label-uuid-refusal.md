# sites-e2fsprogs-label-uuid-refusal

## 判断
tune2fs 参数面与内部拒绝（102 行）：5 行认领（00324/00307/00308/00325/00326——挂载拒绝同向、UUID 形状前置校验）、6 行拒绝（00288-00293 在线改标路径不采纳）、1 行差异（00317 截断 vs 拒绝）、91 行裁剪。全批 103 行已逐行翻态，note 见同名 tsv。

## 依据
SA2-001 同族占用闸（B4/B17）；src/app/decide/edit.rs::precheck 与 ::is_uuid；crates/core/src/fs/attrs.rs::label_problem；tests/cases/mod.rs::set_label_on_ext_refuses_an_over-long_label

## 撤回条件
认领行：占用闸语义或 UUID 校验变化时重审；拒绝行：出现必须在挂载卷上在线改标的外部契约且占用闸允许放行时重审；差异行（00317）：确认存在依赖“超长卷标截断”语义的外部契约、或要求与基线工具逐字节等价改标时，撤回拒绝改为工具同语义；裁剪行：扩大 attrs 参数面时重审。
