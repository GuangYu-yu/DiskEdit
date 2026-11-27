# 02-incidents/inc-libfdisk（17 行）

## 判断
- 裁剪 40：util-linux 全家桶公告（cfdisk/mkfs 包装器/agetty/hwclock/mount 等）与 blkid 探测面；本实现不实现交互 TUI、mkfs 包装器、blkid 探测（识别以档案表为唯一事实源，SA1-017）。
- 拒绝 1（00030）：gpt PMBR 读取越界为 C 缓冲缺陷，防御由 Rust 边界检查承担（GPT-007/010 解析带界）。

## 依据
- 参考行原文（util-linux NEWS）均为依赖/兄弟组件行为；分区面等价行为（无表不崩溃）由 SA1-024 承接。

## 撤回条件
- 若新增交互 TUI 或 blkid 式探测面，重开 00001/00003/00025-00029；若依赖工具版本面变更（SA2-011），重看 00018/00030。

## 换版重放补充（2026-11-27，util-linux 2.39.3→2.42.4）

### 判断
- 裁剪 1（libfdisk-inc-00017）：nsenter cgroup.procs fd 泄漏修复（CVE-2026-78408）属 util-linux 兄弟组件（cgroup 命名空间），不在 libfdisk/分区表面；同 00004 全家桶公告裁剪先例。

### 依据
- sources.tsv 的 libfdisk 范围仅 libfdisk/** + disk-utils/{sfdisk,fdisk,cfdisk}.c；v2.42.4-ReleaseNotes:34 为 nsenter 小节。

### 撤回条件
- 无（nsenter 永不在分区表职责面；若账本范围改为整个 util-linux 则由主控重划批次）。
