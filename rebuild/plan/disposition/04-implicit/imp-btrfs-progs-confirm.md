# 处置说明：btrfs confirm（property/device/tune/check force）

- 批次：`04-implicit/imp-btrfs-progs-confirm`（参考账本 implicit）

## 判断

device/mkfs 覆写检查认领（SA1-005/SA2-012）；挂载 --force 检查旁路拒绝（SA1-004）；property/seeding/超时交互/repair 交互/lowmem 内部分支裁剪。

## 依据

SA1-004/SA1-005/SA2-006/SA2-012。

## 撤回条件

撤回条件：若引入 btrfs property/seeding 或 repair 交互面，先补档案表登记与破坏性确认协议。

## 换版重放补充（2026-11-27，btrfs-progs 6.6.3→7.3）

### 判断
- 认领 4（00024/00042/00043/00044）：device/mkfs 的 -f/--force 覆写选项与 force_overwrite 分支 ↔ 覆写闸（写前拒绝+显式确认，SA1-005/SA2-012）；00043 的挂载检查旁路本身拒绝（SA1-004，同既有拒绝行 00060）。
- 裁剪 5（00006/00007/00034/00061/00063）：property set ro→rw 带 received_uuid 面（00006/00007）、device remove 安全超时交互（00034）、check --force 装配（00061/00063，check 不在扩缩链 SA2-007）。

### 依据
- 覆写检查认领同既有锚行 00036（test_dev_for_mkfs/test_status_for_mkfs）；SA1-004/SA1-005/SA2-012。

### 撤回条件
- 若引入 btrfs property/device remove 或 check 交互面，先补档案表登记与破坏性确认协议。
