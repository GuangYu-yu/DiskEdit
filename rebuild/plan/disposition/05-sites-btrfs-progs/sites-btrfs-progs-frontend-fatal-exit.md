# sites-btrfs-progs-frontend-fatal-exit

## 判断
show/defrag/device scan 的 usage 与 exit(1) 出口（5 行，全部裁剪）。全批已逐行翻态，note 见同名 tsv。
绑定未消费子命令（档案表只登记 resize/check/attrs/mkfs）。

## 依据
crates/core/src/fs/catalog.rs。

## 撤回条件
档案表登记相应子命令消费面时重审。
