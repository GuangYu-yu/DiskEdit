# 处置说明：lvm2 confirm（force/yes_no_prompt）

- 批次：`04-implicit/imp-lvm2-confirm`（参考账本 implicit）

## 判断

grow 步序调用认领（SA2-017）；pvcreate/pvremove/vgchange/raid/lvreduce/remove 等确认面裁剪（LVM 纳入面仅 --grow-lv）。

## 依据

SA2-017；crates/core/src/fs/catalog.rs。

## 撤回条件

撤回条件：若扩展 LVM 能力面（remove/reduce/raid），先扩档案表 Profile 并补真内核用例（s23 同款）。
