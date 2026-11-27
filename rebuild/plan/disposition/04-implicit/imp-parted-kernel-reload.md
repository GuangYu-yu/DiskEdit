# 处置说明：parted kernel-reload（BLKRRPART/BLKPG）

- 批次：`04-implicit/imp-parted-kernel-reload`（参考账本 implicit）

## 判断

Linux 侧 BLKRRPART/BLKPG 全部认领（SA1-013）；gnu.c 两行（Hurd 平台）裁剪。

## 依据

SA1-013；src/app/decide/step.rs::RefreshKernelView；src/platform/linux/mod.rs::refresh_partitions、BLKPG_RESIZE_PARTITION。

## 撤回条件

撤回条件：若纳入 GNU/Hurd 等非 Linux 平台后端，重新评估 gnu.c 行并把平台闸写进 platform 抽象测试。
