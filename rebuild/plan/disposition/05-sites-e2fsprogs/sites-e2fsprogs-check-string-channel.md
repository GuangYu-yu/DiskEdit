# sites-e2fsprogs-check-string-channel

## 判断
e2fsck DEBUG/#ifdef 打印与内部 puts（7 行）。全批 7 行已逐行翻态，note 见同名 tsv。

## 依据
SA2-001/002

## 撤回条件
自研检查器或依赖该调试输出的外部契约出现时重审。
