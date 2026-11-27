# sites-e2fsprogs-check-abort

## 判断
e2fsck sigcatcher 的 abort()。全批 1 行已逐行翻态，note 见同名 tsv。

## 依据
SA2-001/002；src/platform/linux/tools.rs::check_arguments

## 撤回条件
本实现改为自研 e2fsck（不再以外部工具为事实源）或 SA2-001/002 的“只消费退出码+回读”边界被推翻时重审。
