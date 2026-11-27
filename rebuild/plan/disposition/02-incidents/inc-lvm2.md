# 02-incidents/inc-lvm2（98 行）

## 判断
- 裁剪 86：LVM2 WHATS_NEW 条目多为 cluster/lvmetad/dmeventd/thin/cache/raid/mirror/metadata 内部；本实现 LVM 链仅 pvresize→lvextend -r→pvs/vgs 证据（SA2-017），工具内部随发行版版本固定（SA2-011）。
- 认领 1（00081）："A pvresize implementation"——pvresize 正是本实现吸收新分区尺寸的依赖工具（SA2-017/B28）。

## 依据
- 参考行原文（LVM2 WHATS_NEW）均为上游程序内部行为；00067（fsadm 联动）等价契约由 lvextend -r+回读证实承接（SA2-017/SA2-011）。

## 撤回条件
- 若引入 thin/cache/raid/cluster 等拓扑面，重开对应行；依赖版本变更（SA2-011）时复查。
