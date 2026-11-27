# 03-docs/docs-xfsprogs（2 行）

## 判断
- 认领 1（00001）：xfs_growfs 只增不减、按挂载点、须先有新空间——SA2-004/005 与 tests/online.rs 顺序锚定（表写→BLKPG→xfs_growfs）。
- 裁剪 1（00002）：mkfs.xfs 原型文件属上游选项面；本实现 mkfs 固定参数调用（SA2-012）。

## 撤回条件
- 若 mkfs.xfs 选项面（原型文件等）暴露到本实现命令面，重开 00002。
