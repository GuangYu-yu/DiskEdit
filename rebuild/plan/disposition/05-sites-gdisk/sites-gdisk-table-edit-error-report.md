# 处置说明：gdisk table-edit error-report（打开分类/共享锁/扇区回退）

- 批次：`05-sites-gdisk/sites-gdisk-table-edit-error-report`（参考账本 sites/gdisk）

## 判断

打开失败分类、用法出口、close 失败、512 回退、文件尺寸校验认领；共享锁写拒绝（写必须独占）；多平台/Windows/分配失败行裁剪或拒绝。

## 依据

SA1-004/SA1-005/SA1-009/SA1-011；src/platform/linux/mod.rs、src/app/report/exit.rs。

## 撤回条件

撤回条件：若纳入非 Linux 后端，重开平台矩阵；若采纳共享锁写需重证占用闸安全性。
