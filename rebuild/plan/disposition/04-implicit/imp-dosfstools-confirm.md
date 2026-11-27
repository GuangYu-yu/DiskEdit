# 处置说明：dosfstools confirm（交互问答/fsck.fat 模式）

- 批次：`04-implicit/imp-dosfstools-confirm`（参考账本 implicit）

## 判断

整批裁剪：交互问答与 fsck.fat 交互修复模式不纳入；检查经档案表固定只读参数。

## 依据

SA2-012；src/app/cli/parse.rs::--yes。

## 撤回条件

撤回条件：若为 vfat 提供修复应答面，先登记 FsckVfat 参数与退出码矩阵并补真内核用例。
