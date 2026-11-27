# 处置说明：gdisk mbr refusal（扩展链/EBR/写路径）

- 批次：`05-sites-gdisk/sites-gdisk-mbr-refusal`（参考账本 sites/gdisk）

## 判断

扩展链/EBR 校验、写失败上抛、首空槽认领（SA1-011/SA1-027）；越界钳制行认领为本实现更保守（越界拒绝，SA1-014）；RemoveLogicalsFromFirstFour 裁剪（SA3-002）；非法包含 exit 拒绝。

## 依据

SA1-009/SA1-011/SA1-014/SA1-027/SA3-002。

## 撤回条件

撤回条件：若采纳钳制语义，需先证明拒绝语义造成兼容性断裂。
