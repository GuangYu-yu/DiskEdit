# 处置说明：gdisk misc refusal

- 批次：`05-sites-gdisk/sites-gdisk-misc-refusal`（参考账本 sites/gdisk）

## 判断

属性未知运算符拒绝、扇区大小探测认领；fixparts 行裁剪。

## 依据

SA1-004/SA3-001；crates/core/src/table/edit.rs、src/platform/linux/mod.rs::block_sector_size。

## 撤回条件

撤回条件：同 fixparts。
