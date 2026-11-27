# sites-xfsprogs-mkfs-fatal-exit

## 判断
mkfs.xfs usage/OOM/fail() 致命出口（125 行，全部裁剪）。全批已逐行翻态，note 见同名 tsv。
固定参数调用下 usage 不可达；非零退出码统一按 mkfs 失败消费。

## 依据
src/app/execute/filesystem.rs::mkfs_invocation。

## 撤回条件
mkfs 参数面扩展导致 usage 出口可达时重审。

## 换版重放补充（xfsprogs 6.6.0→7.3.0）

换版重放在本批新增/改写 77 行（state=待判），已全部翻态：裁剪 77；note 见同名 tsv。
- 判断：新增/改写行仍属 mkfs.xfs usage/OOM/fail() 致命出口（新选项面扩大但固定 `mkfs.xfs -f [-L]` 调用不变）——usage 不可达，非零退出码统一按 mkfs 失败消费。
- 依据：src/app/execute/filesystem.rs::mkfs_invocation。
- 撤回条件：mkfs 参数面扩展导致 usage 出口可达时重审。
