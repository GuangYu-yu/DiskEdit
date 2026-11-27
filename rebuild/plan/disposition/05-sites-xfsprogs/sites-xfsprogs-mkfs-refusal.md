# sites-xfsprogs-mkfs-refusal

## 判断
mkfs.xfs 自身 CLI/几何选项/proto 文件面（177 行，全部裁剪）。全批已逐行翻态，note 见同名 tsv。
固定 `mkfs.xfs -f [-L label]` 调用（src/app/execute/filesystem.rs::mkfs_invocation）：选项解析、几何选项、-p proto 文件面不可达或随外部工具自带；SA2 未定案 mkfs 内部面，不逐点认领。

## 依据
src/app/execute/filesystem.rs::mkfs_invocation、crates/core/src/fs/catalog.rs（mkfs=Tools(MkfsXfs)）。

## 撤回条件
mkfs 参数面扩展（-d/-l/-r/-i 等成为消费面）或出现依赖 mkfs.xfs 拒绝原文的外部契约时重审。

## 换版重放补充（xfsprogs 6.6.0→7.3.0）

换版重放在本批新增/改写 150 行（state=待判），已全部翻态：裁剪 150；note 见同名 tsv。
- 判断：新增/改写行仍属 mkfs.xfs 自身 CLI/几何选项/proto 文件面；上游 7.3.0 的选项约束链（metadir/exchrange/parent pointers/autofsck/persistent quota 组合约束、zoned RT 与 rgsize/rgcount、max_atomic_write 上限、-c defaults 配置文件、proto metadir/rtgroup inode 路径）在固定调用下不可达或随外部工具自带。
- 依据：src/app/execute/filesystem.rs::mkfs_invocation；crates/core/src/fs/catalog.rs（mkfs=Tools(MkfsXfs)）。
- 撤回条件：mkfs 参数面扩展（-d/-l/-r/-i/-c 等成为消费面）或出现依赖 mkfs.xfs 拒绝原文的外部契约时重审。
