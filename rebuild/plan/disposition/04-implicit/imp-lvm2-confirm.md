# 处置说明：lvm2 confirm（force/yes_no_prompt）

- 批次：`04-implicit/imp-lvm2-confirm`（参考账本 implicit）

## 判断

grow 步序调用认领（SA2-017）；pvcreate/pvremove/vgchange/raid/lvreduce/remove 等确认面裁剪（LVM 纳入面仅 --grow-lv）。

## 依据

SA2-017；crates/core/src/fs/catalog.rs。

## 撤回条件

撤回条件：若扩展 LVM 能力面（remove/reduce/raid），先扩档案表 Profile 并补真内核用例（s23 同款）。

## 换版重放补充（2026-11-27，LVM2 →2.03.44）

### 判断
- 认领 4（00001/00060/00061/00062）：lvresize force 与 fsadm_cmd CHECK/RESIZE 装配 ↔ 本实现固定步序 写表→pvresize→lvextend -r，直接 lvextend -r 固定参数，不传递 force/yes 旁路（SA2-017，同既有锚行 00002/00048）；fs 检查退出码契约由 SA2-001 承接。
- 裁剪 30：vgchange 交互确认族（00004-00006/00010/00011/00013/00014）、lvmcmdline force 等级/交互模式/cluster 访问（00015-00017/00021/00022）、toollib pvcreate force（00025/00027）、lv_manip fs 步骤交互确认族（00051-00055/00056-00059/00067）、lvremove 在用检查（00085/00086）、activate_and_wipe 擦除面（00093-00096，本实现从不擦除，SA1-005/SA1-006）——LVM 纳入面仅 --grow-lv，交互应答类裁剪。

### 依据
- SA2-017；src/app/cli/parse.rs::--yes；crates/core/src/fs/catalog.rs。

### 撤回条件
- 若扩展 LVM 能力面（vgchange/lvremove/reduce/wipe/raid），先扩档案表 Profile 并补真内核用例（s23 同款）。
