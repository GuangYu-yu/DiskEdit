# 处置说明：xfsprogs default（mkfs 内部常数）

- 批次：`04-implicit/imp-xfsprogs-default`（参考账本 implicit）

## 判断

整批裁剪：mkfs.xfs 布局常数不复制，xfs 面只登记 growfs。

## 依据

SA2-004/SA2-005。

## 撤回条件

撤回条件：若引入 xfs 建卷能力，常数在 mkfs.xfs 内部承担，仍无需复制。

## 换版重放补充（2026-11-27，xfsprogs 6.6.0→7.3.0）

### 判断
- 裁剪 3（00001/00002/00012）：mkfs.xfs 内部常数（proto 硬链接增长因子、XFS_MIN_REALISTIC_LOG_BLOCKS、MAX_RW_COUNT）；xfs 面只登记 growfs（SA2-004/005），mkfs 以档案表固定参数调用（SA2-012），常数不复制。

### 依据
- mkfs/proto.c:1332、mkfs/xfs_mkfs.c:32/5130 均为 mkfs 内部；与既有整批裁剪口径一致。

### 撤回条件
- 若引入 xfs 建卷能力，常数在 mkfs.xfs 内部承担，仍无需复制；若引入 xfs 只读探针则另立探针行。
