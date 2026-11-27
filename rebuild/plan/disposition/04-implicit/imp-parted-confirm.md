# 处置说明：parted confirm（交互模式）

- 批次：`04-implicit/imp-parted-confirm`（参考账本 implicit）

## 判断

整批裁剪：交互模式不纳入；等价语义=非交互 CLI+判定层闸门+--yes。

## 依据

SA1-005；src/app/cli/parse.rs::--yes。

## 撤回条件

撤回条件：若引入交互式提示面，先定义确认协议（逐项 vs 单点）与不可回退操作清单。
