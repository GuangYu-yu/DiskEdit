# 处置说明：e2fsprogs confirm（force/挂载检查/交互应答）

- 批次：`04-implicit/imp-e2fsprogs-confirm`（参考账本 implicit）

## 判断

mke2fs -F 覆写闸、挂载检查、-f 强制检查、interactive 判定认领（SA1-004/SA1-005/SA2-001）；tune2fs/dumpe2fs/undo/journal/交互应答/终端要求裁剪。

## 依据

SA1-004/SA1-005/SA2-001；src/app/execute/filesystem.rs:966（-fp）；src/app/decide/refusal.rs::Occupied。

## 撤回条件

撤回条件：若引入交互修复应答面（-y/-n 预答可配置），先在档案表登记参数与退出码矩阵并补真内核用例。
