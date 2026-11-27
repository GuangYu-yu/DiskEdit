# sites-btrfs-progs-check-fatal-exit

## 判断
check 自身 usage/修复模式/内部致命出口（12 行，全部裁剪）。全批已逐行翻态，note 见同名 tsv。
本实现固定 `btrfs check --readonly`：--repair/--force/--clear-* 出口不可达；任何非零退出码统一按 check 失败映射（diagnose.rs::run）。

## 依据
src/platform/linux/tools.rs::check_arguments（check --readonly）、src/app/execute/diagnose.rs::run。

## 撤回条件
决定提供 btrfs 修复写路径或 check 参数面变化时重审。
