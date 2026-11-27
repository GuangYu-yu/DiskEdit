#6 定案证据：ext 收缩 `resize2fs -P` 最小尺寸写前闸
日期: 2026-11-27
结论: 认领真 —— 旧工具 `resize2fs -P` 确实给出最小尺寸估计，参考闸按其比较；本实现补构造点。

## 真值来源（按 plan/baseline/reference/sources.tsv）
- 版本: e2fsprogs 1.47.4
- URL: https://mirrors.edge.kernel.org/pub/linux/kernel/people/tytso/e2fsprogs/v1.47.4/e2fsprogs-1.47.4.tar.xz
- sha256: fd5bf388cbdbe006a3d3b318d983b2948382440acc85a87f1e7d108653e8db0b（与 sources.tsv 一致）
- 构建: ./configure --disable-defrag --disable-elf-shlibs && make -j
- 版本串: resize2fs 1.47.4 (6-Mar-2025)

## 实跑（普通 ext 镜像文件，无需 root）
命令:
  truncate -s 512M ext.img
  mke2fs -t ext4 -F -q ext.img
  resize2fs -P ext.img
输出:
  resize2fs 1.47.4 (6-Mar-2025)
  Estimated minimum size of the filesystem: 6366
dumpe2fs -h ext.img:
  Block size: 4096
推得最小字节 = 6366 * 4096 = 26,074,368 字节。

## 判定与落点
- 参考在三处（旧 cmd/layout.rs:1067、cmd/resize/dry_run.rs:257、cmd/resize/msdos.rs:532，
  对应 R0211/R0382/R0416）按同一判据拒绝：target size < estimated minimum FS size。
- 本实现新架构把三处收敛为一条离线收缩判定：
  - 采集: src/app/observe/resize.rs::offline_min_bytes → 只读探针 resize2fs -P × dumpe2fs -h
  - 判定: src/app/decide/resize.rs::decide → Refusal::BelowMinimumSize / MinimumSizeUnknown
- 单测: tests/online.rs::offline_ext_shrink_{below,above,unreadable_*}
