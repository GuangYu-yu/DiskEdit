# sites-btrfs-progs-misc-refusal

## 判断
btrfstune 未消费特性的拒绝面（89 行，全部裁剪）。全批已逐行翻态，note 见同名 tsv。
绑定 change-csum（清 csum）、convert-bgt（块组树转换）、quota rescan、seeding、tune CLI/usage——档案表消费面只登记 resize/check/attrs/mkfs，特性绑定不存在。

## 依据
crates/core/src/fs/catalog.rs。

## 撤回条件
本实现接入上述 btrfstune 特性（如块组树转换、quota rescan）时重审。

## 换版重放补充（btrfs-progs 6.6.3→7.3）

换版重放在本批新增/改写 9 行（state=待判），已全部翻态：裁剪 9；note 见同名 tsv。
- 判断：新增行仍属 btrfstune 未消费特性（change-csum 换校验算法、convert-remap-tree/remap-tree 新特性、replace/balance 闸）的拒绝面，档案表消费面未登记。
- 依据：crates/core/src/fs/catalog.rs（消费面只登记 resize/check/attrs/mkfs）。
- 撤回条件：本实现接入上述 btrfstune 特性（如 remap-tree 转换、change-csum）时重审。
