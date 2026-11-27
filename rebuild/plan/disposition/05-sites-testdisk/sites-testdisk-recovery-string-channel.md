# 处置说明：testdisk recovery string-channel（挂载提示）

- 批次：`05-sites-testdisk/sites-testdisk-recovery-string-channel`（参考账本 sites/testdisk）

## 判断

裁剪：ncurses 提示面；等价语义=占用闸拒绝+拒绝文本。

## 依据

SA1-004；src/app/decide/refusal.rs::Occupied。

## 撤回条件

撤回条件：若用户实测提示不足，把占用拒绝文本补全为提示面工作项（提示面缺失标签）。
