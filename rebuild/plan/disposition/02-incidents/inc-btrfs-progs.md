# 02-incidents/inc-btrfs-progs（72 行）

## 判断
- 裁剪 67：全部为依赖工具 btrfs-progs（check/convert/inspect/send/receive/quota/scrub/image/restore/mkfs）内部修复/特性；本实现仅按档案表以 btrfs resize 承接在线扩缩（SA2-006/007/008/016）及登记的 mkfs/attrs/check 面（SA2-012），工具内部随发行版版本固定（SA2-011）。

## 依据
- 参考行原文（btrfs-progs CHANGES）均为上游程序内部行为；00034（check 挂载检测）的等价防线是本实现自建占用闸（SA1-004），不依赖上游。

## 撤回条件
- 若 btrfs check --repair 或多设备面纳入档案表，重开对应行；依赖版本变更（SA2-011）时复查。
