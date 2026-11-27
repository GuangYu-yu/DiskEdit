# 02-incidents/inc-btrfs-progs（72 行）

## 判断
- 裁剪 67：全部为依赖工具 btrfs-progs（check/convert/inspect/send/receive/quota/scrub/image/restore/mkfs）内部修复/特性；本实现仅按档案表以 btrfs resize 承接在线扩缩（SA2-006/007/008/016）及登记的 mkfs/attrs/check 面（SA2-012），工具内部随发行版版本固定（SA2-011）。

## 依据
- 参考行原文（btrfs-progs CHANGES）均为上游程序内部行为；00034（check 挂载检测）的等价防线是本实现自建占用闸（SA1-004），不依赖上游。

## 撤回条件
- 若 btrfs check --repair 或多设备面纳入档案表，重开对应行；依赖版本变更（SA2-011）时复查。

## 换版重放补充（2026-11-27，btrfs-progs 6.6.3→7.3）

### 判断
- 裁剪 6（00001-00005、00016）：CHANGES 条目——corrupt-block 调试工具（00001/00003）、check 内部检测/修复（00002/00005）、balance filter（00004）、receive 崩溃修复（00016）；全部不在依赖面，本实现仅按档案表以 btrfs resize 承接在线扩缩（SA2-006/007/008/016）。00001 重新锚定全批 note（旧锚行被重放覆盖）。

### 依据
- 参考行原文（CHANGES:12/14/89/101/328/788）为上游程序内部行为。

### 撤回条件
- 若 btrfs check --repair、balance、receive 或多设备面纳入档案表，重开对应行；依赖版本变更（SA2-011）时复查。
