# 处置说明：gdisk gpt refusal（校验/命令面/备份文件）

- 批次：`05-sites-gdisk/sites-gdisk-gpt-refusal`（参考账本 sites/gdisk）

## 判断

头/表校验、修复矩阵、命令面校验（add/delete/set/属性位）、未知选项拒绝、错误即不保存认领；备份文件/专家表搬移/盘标转换/分区交换/菜单面裁剪；分配失败拒绝。

## 依据

SA1-005/SA1-007/SA1-010/SA1-020/SA3-002/SA3-003；crates/core/src/table/edit.rs、gpt/parse.rs、src/app/cli/parse.rs。

## 撤回条件

撤回条件：盘标转换或分区交换若纳入，先补身份重派生（SA1-007）与回读用例。
