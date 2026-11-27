# 处置说明：testdisk confirm（MBR code 写入确认）

- 批次：`04-implicit/imp-testdisk-confirm`（参考账本 implicit）

## 判断

裁剪：MBR 引导代码写入特性未纳入；写面=表结构（PMBR 规范化/CHS 占位）。

## 依据

SA1-003/SA1-019/SA1-027。

## 撤回条件

撤回条件：出现需要重装引导代码的外部契约时，另立写点+回读工作项，不得并入表写通道。
