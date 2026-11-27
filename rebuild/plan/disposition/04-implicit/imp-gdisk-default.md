# 处置说明：gdisk default（常数群）

- 批次：`04-implicit/imp-gdisk-default`（参考账本 implicit）

## 判断

签名/几何/1MiB 对齐/4KiB 最小对齐/Linux 类型认领；平台默认类型（solaris/mac/win/bsd）、专家对齐上限、混合 MBR 尺寸分级、卫哨宏裁剪。

## 依据

SA1-008/SA1-016/SA3-002；crates/core/src/table/edit.rs、mbr、platform/linux::block_sector_size。

## 撤回条件

撤回条件：若引入多平台默认类型或专家对齐调参，先补类型集与对齐策略的用例矩阵。
