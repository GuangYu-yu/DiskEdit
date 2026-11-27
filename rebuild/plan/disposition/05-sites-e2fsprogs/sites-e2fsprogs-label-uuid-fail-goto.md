# sites-e2fsprogs-label-uuid-fail-goto

## 判断
tune2fs 内部失败清理（32 行）。全批 32 行已逐行翻态，note 见同名 tsv。

## 依据
src/app/execute/filesystem.rs::run_checked（非零退出即失败）

## 撤回条件
同上。
