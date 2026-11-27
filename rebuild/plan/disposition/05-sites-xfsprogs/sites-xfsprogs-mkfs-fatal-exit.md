# sites-xfsprogs-mkfs-fatal-exit

## 判断
mkfs.xfs usage/OOM/fail() 致命出口（125 行，全部裁剪）。全批已逐行翻态，note 见同名 tsv。
固定参数调用下 usage 不可达；非零退出码统一按 mkfs 失败消费。

## 依据
src/app/execute/filesystem.rs::mkfs_invocation。

## 撤回条件
mkfs 参数面扩展导致 usage 出口可达时重审。
