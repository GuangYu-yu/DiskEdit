# sites-xfsprogs-mkfs-string-channel

## 判断
mkfs.xfs " FAILED (%d)" 管道打印（换版重放后全批 1 行，裁剪）。note 见同名 tsv。
随外部工具自带：固定 `mkfs.xfs -f [-L label]` 调用只消费退出码，不复刻过程文本。

## 依据
src/app/execute/filesystem.rs::mkfs_invocation。

## 撤回条件
出现依赖 mkfs.xfs 过程文本的外部契约或 mkfs 参数面扩展时重审。
