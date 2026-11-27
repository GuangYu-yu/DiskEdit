# 处置说明：libfdisk default（常数群）

- 批次：`04-implicit/imp-libfdisk-default`（参考账本 implicit）

## 判断

GPT/MBR 常数、对齐语义、默认类型 GUID、128 条目认领；ask.c 问答宏裁剪。

## 依据

SA1-008/SA1-014/SA1-019/SA3-002；crates/core/src/table/edit.rs、gpt/parse.rs。

## 撤回条件

撤回条件：无（纯常数行，行为面已由认领项承接）。
