# sites-btrfs-progs-misc-refusal

## 判断
btrfstune 未消费特性的拒绝面（89 行，全部裁剪）。全批已逐行翻态，note 见同名 tsv。
绑定 change-csum（清 csum）、convert-bgt（块组树转换）、quota rescan、seeding、tune CLI/usage——档案表消费面只登记 resize/check/attrs/mkfs，特性绑定不存在。

## 依据
crates/core/src/fs/catalog.rs。

## 撤回条件
本实现接入上述 btrfstune 特性（如块组树转换、quota rescan）时重审。
