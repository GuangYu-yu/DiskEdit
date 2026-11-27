# 处置说明：lvm2 lock（O_DIRECT/O_EXCL/fsync）

- 批次：`04-implicit/imp-lvm2-lock`（参考账本 implicit）

## 判断

O_EXCL 认领（SA1-004/B5）、fsync 认领（SA1-011）；O_DIRECT 系裁剪。

## 依据

SA1-004/SA1-011。

## 撤回条件

撤回条件：同 imp-btrfs-progs-lock。
