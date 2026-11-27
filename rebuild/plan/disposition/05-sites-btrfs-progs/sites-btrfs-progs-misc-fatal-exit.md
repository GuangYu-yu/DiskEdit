# sites-btrfs-progs-misc-fatal-exit

## 判断
tune 子命令 usage 致命出口（3 行，全部裁剪）。全批已逐行翻态，note 见同名 tsv。
btrfstune 未消费特性面（change-csum/convert-bgt/quota/seeding/CLI），档案表未登记。

## 依据
crates/core/src/fs/catalog.rs（消费面只登记 resize/check/attrs/mkfs）。

## 撤回条件
档案表登记相应 btrfstune 特性时重审。
