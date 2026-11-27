# 处置说明：xfsprogs confirm（force_overwrite/xfs_db）

- 批次：`04-implicit/imp-xfsprogs-confirm`（参考账本 implicit）

## 判断

mkfs.xfs 覆写检查认领（SA1-005/SA2-012）；xfs_db 调试器 -F 裁剪。

## 依据

SA1-005/SA2-004/005/012；src/app/decide/filesystem.rs::Occupied。

## 撤回条件

撤回条件：若引入 xfs 建卷或调试探针，先补档案表登记与只读访问审计。
