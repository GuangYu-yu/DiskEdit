# sites-xfsprogs-misc-error-report

## 判断
xfs_db usage/无效超块消息（3 行，全部裁剪）。全批已逐行翻态，note 见同名 tsv。
db 工具不在消费面（档案表只登记 growfs；诊断/几何走自研探针与 `xfs_growfs -n` 回读）。

## 依据
crates/core/src/fs/catalog.rs（xfs 行）、src/app/execute/filesystem.rs::verify_xfs_size。

## 撤回条件
本实现接入 xfs_db 或出现依赖 db 输出的外部契约时重审。

## 换版重放补充（xfsprogs 6.6.0→7.3.0）

换版重放在本批新增/改写 1 行（state=待判），已全部翻态：裁剪 1；note 见同名 tsv。
- 判断：xfs_db usage 消息——db 工具不在消费面（档案表只登记 growfs；诊断/几何走自研探针与 `xfs_growfs -n` 回读）。
- 依据：crates/core/src/fs/catalog.rs（xfs 行）；src/app/execute/filesystem.rs::verify_xfs_size。
- 撤回条件：本实现接入 xfs_db 或出现依赖 db 输出的外部契约时重审。
