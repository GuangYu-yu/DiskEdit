# 处置说明：btrfs lock（O_EXCL/O_DIRECT）

- 批次：`04-implicit/imp-btrfs-progs-lock`（参考账本 implicit）

## 判断

O_EXCL 原子独占认领（B5/SA1-004）；O_DIRECT 直通模式裁剪。

## 依据

SA1-004/SA1-011；src/platform/linux/mod.rs（记录文件 O_CREAT|O_EXCL）。

## 撤回条件

撤回条件：若直通 I/O 成为正确性前提（如页缓存污染实测），以仪器化基准重评。
