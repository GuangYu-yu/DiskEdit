# sites-btrfs-progs-check-string-channel

## 判断
check 腐蚀清单/修复提示 printf（3 行，全部裁剪）。全批已逐行翻态，note 见同名 tsv。
随外部工具自带，本实现只消费退出码、不复刻显示面。

## 依据
src/app/execute/diagnose.rs::run。

## 撤回条件
出现依赖 check 显示文本的外部契约时重审。
