# sites-e2fsprogs-mkfs-refusal

## 判断
mke2fs CLI 参数与内部校验（126 行）：1 行差异（00481 mkfs 卷标超长前置拒绝 vs 截断）、3 行特殊裁剪（00606/00613 e2undo、00615 discard）、120 行默认裁剪。全批 124 行已逐行翻态，note 见同名 tsv。

## 依据
src/app/decide/mkfs.rs 的 label_problem 前置闸；crates/core/src/fs/attrs.rs::label_problem；SA1-006（不发送 discard）；事务记录承担崩溃安全

## 撤回条件
差异行（00481）：确认存在依赖“超长卷标截断建卷”语义的外部契约、或要求与基线建卷逐字节等价时，撤回拒绝改为工具同语义；特殊裁剪行：引入工具级 undo/discard 能力时重审；默认裁剪行：扩大 mkfs 参数面时重审。
