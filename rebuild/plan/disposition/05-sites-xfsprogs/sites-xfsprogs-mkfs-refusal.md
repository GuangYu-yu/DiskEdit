# sites-xfsprogs-mkfs-refusal

## 判断
mkfs.xfs 自身 CLI/几何选项/proto 文件面（177 行，全部裁剪）。全批已逐行翻态，note 见同名 tsv。
固定 `mkfs.xfs -f [-L label]` 调用（src/app/execute/filesystem.rs::mkfs_invocation）：选项解析、几何选项、-p proto 文件面不可达或随外部工具自带；SA2 未定案 mkfs 内部面，不逐点认领。

## 依据
src/app/execute/filesystem.rs::mkfs_invocation、crates/core/src/fs/catalog.rs（mkfs=Tools(MkfsXfs)）。

## 撤回条件
mkfs 参数面扩展（-d/-l/-r/-i 等成为消费面）或出现依赖 mkfs.xfs 拒绝原文的外部契约时重审。
