# 处置说明：btrfs default（usage 阈值）

- 批次：`04-implicit/imp-btrfs-progs-default`（参考账本 implicit）

## 判断

裁剪：usage 统计内部阈值；btrfs 事实源=superblock 回读。

## 依据

SA2-006。

## 撤回条件

撤回条件：若引入 btrfs usage 展示，另立探针行。

## 换版重放补充（2026-11-27，btrfs-progs 6.6.3→7.3）

### 判断
- 裁剪 1（00051）：mkfs --rootdir MAX_EXTENT_SIZE（1MiB）内部常数；mkfs 以档案表固定参数调用（SA2-012），btrfs 事实源=superblock 回读（SA2-006）。

### 依据
- mkfs/rootdir.c:732 属 --rootdir 特性内部，不在调用面。

### 撤回条件
- 若引入 btrfs usage/--rootdir 面，另立探针行。
