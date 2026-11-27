# sites-e2fsprogs-mkfs-fatal-exit

## 判断
mke2fs usage/OOM 出口（128 行）。全批 116 行已逐行翻态，note 见同名 tsv。

## 依据
mkfs_invocation 固定参数（-t extN [-L label]），usage 不可达

## 撤回条件
扩大参数面或依赖 usage 码的外部契约出现时重审。
