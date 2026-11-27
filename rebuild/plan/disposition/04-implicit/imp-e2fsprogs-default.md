# 处置说明：e2fsprogs default（常数群）

- 批次：`04-implicit/imp-e2fsprogs-default`（参考账本 implicit）

## 判断

整批裁剪：编译宏/内部布局/调度常数/discard 步进均不复制；discard 另有 SA1-006 范围裁剪。

## 依据

SA1-006/SA2-001/002/003；crates/core/src/fs/catalog.rs。

## 撤回条件

撤回条件：若自研 ext 布局知识（探针级），先补 SA2-003 回读口径的用例。
