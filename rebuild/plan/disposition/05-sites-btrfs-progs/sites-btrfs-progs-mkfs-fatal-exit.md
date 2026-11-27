# sites-btrfs-progs-mkfs-fatal-exit

## 判断
mkfs.btrfs usage/OOM/内部致命出口（14 行，全部裁剪）。全批已逐行翻态，note 见同名 tsv。
固定 `mkfs.btrfs -f [-L]` 调用下 usage 出口不可达；非零退出码统一按 mkfs 失败消费。

## 依据
src/app/execute/filesystem.rs::mkfs_invocation。

## 撤回条件
mkfs 参数面扩展（类型/节点尺寸等）导致 usage 出口可达时重审。

## 换版重放补充（btrfs-progs 6.6.3→7.3）

换版重放在本批新增/改写 4 行（state=待判），已全部翻态：裁剪 4；note 见同名 tsv。
- 判断：新增行仍属 mkfs.btrfs usage/OOM/内部致命出口——固定 `mkfs.btrfs -f [-L]` 调用下不可达，非零退出码统一按 mkfs 失败消费。
- 依据：src/app/execute/filesystem.rs::mkfs_invocation。
- 撤回条件：mkfs 参数面扩展导致 usage 出口可达时重审。
