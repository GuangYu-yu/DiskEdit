# sites-btrfs-progs-mkfs-refusal

## 判断
mkfs.btrfs 自身 CLI/建卷路径校验/覆写检查（113 行，全部裁剪）。全批已逐行翻态，note 见同名 tsv。
固定 `mkfs.btrfs -f [-L label]` 调用（src/app/execute/filesystem.rs::mkfs_invocation）：BOX_MAIN getopt、-r rootdir 填充、check_overwrite、test_dev_for_mkfs 等随外部工具自带；覆写与占用闸由本实现决策层承担（SA1-005），不逐点复刻。

## 依据
SA1-005（覆写闸）、src/app/execute/filesystem.rs::mkfs_invocation、crates/core/src/fs/catalog.rs（mkfs=Tools(MkfsBtrfs)）。

## 撤回条件
mkfs 参数面扩展（-r/-m/-d 等成为消费面）或出现依赖 mkfs.btrfs 拒绝原文的外部契约时重审。

## 换版重放补充（btrfs-progs 6.6.3→7.3）

换版重放在本批新增/改写 121 行（state=待判），已全部翻态：裁剪 121；note 见同名 tsv。
- 判断：新增行仍属 mkfs.btrfs 自身 CLI/建卷路径校验/覆写检查；上游 7.x 新特性面（remap-tree、raid-stripe-tree、block-group-tree 依赖、zoned RAID 约束、-r rootdir 子卷/压缩/硬链接填充、quota/simple accounting）全部绑定固定 `mkfs.btrfs -f [-L label]` 未消费的选项面。
- 依据：src/app/execute/filesystem.rs::mkfs_invocation；SA1-005（覆写与占用闸由本实现决策层承担）。
- 撤回条件：mkfs 参数面扩展（-r/-m/-d 等成为消费面）或出现依赖 mkfs.btrfs 拒绝原文的外部契约时重审。
