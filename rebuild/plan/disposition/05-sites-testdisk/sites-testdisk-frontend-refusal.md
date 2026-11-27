# 处置说明：testdisk frontend refusal（MBR/扩展链校验+老盘标）

- 批次：`05-sites-testdisk/sites-testdisk-frontend-refusal`（参考账本 sites/testdisk）

## 判断

MBR 结构/扩展链/重叠/写失败行认领（crates/core/src/table/mbr、SA1-011）；humax/mac/sun/xbox/bsd 及内容级校验裁剪（SA1-016/SA1-017）。

## 依据

SA1-011/SA1-016/SA1-017；crates/core/src/table/mbr。

## 撤回条件

撤回条件：老盘标与内容级识别同 SA1-016/SA1-017 撤回条件。
