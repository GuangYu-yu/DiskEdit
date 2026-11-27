# sites-xfsprogs-misc-fatal-exit

## 判断
xfs_db usage/致命出口（8 行，全部裁剪）。全批已逐行翻态，note 见同名 tsv。
db 工具不在消费面。

## 依据
crates/core/src/fs/catalog.rs。

## 撤回条件
同上：接入 xfs_db 时重审。
