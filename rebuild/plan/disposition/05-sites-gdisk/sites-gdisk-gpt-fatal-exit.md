# 处置说明：gdisk gpt fatal-exit（exit 出口）

- 批次：`05-sites-gdisk/sites-gdisk-gpt-fatal-exit`（参考账本 sites/gdisk）

## 判断

分配失败 exit 拒绝（运行时 abort 承担，架构差异-多虑）；分区号越界 exit 认领（越界拒绝语义已有，以 Refusal 表达）；交互取数 exit 与备份文件装载 exit 裁剪/拒绝。

## 依据

SA1-009/SA1-020；src/app/report/exit.rs::USAGE。

## 撤回条件

撤回条件：无（exit 风格由退出码单源承接）。
