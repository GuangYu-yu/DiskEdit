# 处置说明：xfsprogs default（mkfs 内部常数）

- 批次：`04-implicit/imp-xfsprogs-default`（参考账本 implicit）

## 判断

整批裁剪：mkfs.xfs 布局常数不复制，xfs 面只登记 growfs。

## 依据

SA2-004/SA2-005。

## 撤回条件

撤回条件：若引入 xfs 建卷能力，常数在 mkfs.xfs 内部承担，仍无需复制。
