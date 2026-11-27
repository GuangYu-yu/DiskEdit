# 02-incidents/inc-lvm2（98 行）

## 判断
- 裁剪 86：LVM2 WHATS_NEW 条目多为 cluster/lvmetad/dmeventd/thin/cache/raid/mirror/metadata 内部；本实现 LVM 链仅 pvresize→lvextend -r→pvs/vgs 证据（SA2-017），工具内部随发行版版本固定（SA2-011）。
- 认领 1（00081）："A pvresize implementation"——pvresize 正是本实现吸收新分区尺寸的依赖工具（SA2-017/B28）。

## 依据
- 参考行原文（LVM2 WHATS_NEW）均为上游程序内部行为；00067（fsadm 联动）等价契约由 lvextend -r+回读证实承接（SA2-017/SA2-011）。

## 撤回条件
- 若引入 thin/cache/raid/cluster 等拓扑面，重开对应行；依赖版本变更（SA2-011）时复查。

## 换版重放补充（2026-11-27，LVM2 →2.03.44）

### 判断
- 裁剪 16（00001-00011、00014、00028、00038、00051、00082）：WHATS_NEW 聚合块——VDO/thin/dmeventd/lvmpolld/lvmlockd/raid/mirror/pvck/writecache/集群与老版本（2.02.x）聚合；全部不在 PV 链依赖面（本实现仅 pvresize→lvextend -r→pvs/vgs 证据，SA2-017）。00001 重新锚定全批 note（旧锚行被重放覆盖）。00008（lvresize -r LV→crypt→FS 栈、btrfs 支持增强）与 00014（fsadm fsck 结果处理）属上游内部，等价契约由 SA2-017/SA2-001 承接。

### 依据
- 参考行原文（WHATS_NEW 各版本块）为上游程序内部行为；00008/00014 与既有认领行 00081（pvresize）、00048（fsadm 装配）口径一致。

### 撤回条件
- 若引入 thin/cache/raid/cluster/VDO 等拓扑面或 lvresize -r 的 btrfs 内层扩，重开对应行；依赖版本变更（SA2-011）时复查。
