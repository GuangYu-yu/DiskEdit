# 处置说明：btrfs confirm（property/device/tune/check force）

- 批次：`04-implicit/imp-btrfs-progs-confirm`（参考账本 implicit）

## 判断

device/mkfs 覆写检查认领（SA1-005/SA2-012）；挂载 --force 检查旁路拒绝（SA1-004）；property/seeding/超时交互/repair 交互/lowmem 内部分支裁剪。

## 依据

SA1-004/SA1-005/SA2-006/SA2-012。

## 撤回条件

撤回条件：若引入 btrfs property/seeding 或 repair 交互面，先补档案表登记与破坏性确认协议。
