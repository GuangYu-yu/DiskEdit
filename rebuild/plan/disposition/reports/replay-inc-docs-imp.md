# 换版重放处置报告：02-incidents / 03-docs / 04-implicit（2026-11-27）

参考账本换版重放（parted 3.8 / util-linux 2.42.4 / e2fsprogs 1.47.4 / xfsprogs 7.3.0 / btrfs-progs 7.3 / LVM2 2.03.44 / ntfs-3g 2026.9.28）后，本三目录 196 个 state=待判 行全部翻态。依据 `PROTOCOL.md` 四态定义与 `plan/baseline/incident-classes.tsv`（SA1/SA2/SA3 各条）；未改动 queue.tsv，未改任何代码。

## 一、四态计数（本报告范围 196 行）

| 目录 | 认领 | 拒绝 | 差异 | 裁剪 | 小计 |
| --- | --- | --- | --- | --- | --- |
| 02-incidents | 0 | 1 | 0 | 71 | 72 |
| 03-docs | 3 | 0 | 0 | 66 | 69 |
| 04-implicit | 10 | 0 | 0 | 45 | 55 |
| **合计** | **13** | **1** | **0** | **182** | **196** |

分批明细：
- 02-incidents：parted 裁剪1+拒绝1；libfdisk 裁剪1；e2fsprogs 裁剪38；xfsprogs 裁剪9；btrfs-progs 裁剪6；lvm2 裁剪16。
- 03-docs：btrfs-progs 认领3（doc-00034/00035/00037）+裁剪54；lvm2 裁剪12。
- 04-implicit：认领 10 = e2fsprogs-prompt 1（imp-00291 同 00037 锚）+ xfsprogs-confirm 1（imp-00011）+ btrfs-confirm 4（imp-00024/00042/00043/00044）+ lvm2-confirm 4（imp-00001/00060/00061/00062）；其余 45 行裁剪。

## 二、待实现工作项

无。全部 13 行认领均引既有 SA id 与测试锚，属已实现行为（占用闸/覆写闸/superblock 回读/固定步序/固定 -fp 预答），不产生新工作项。

## 三、差异建议（D-建议）

无新增差异行。上游换版未产生与"同为范围内但行为不同"的对照点：T1 分区表面（parted/libfdisk）本轮新增条目全部为 C 内存/崩溃修复或兄弟组件公告，不触及唯一写点、回读证实、占用闸、退出码单源、对称回读五个已收敛根因族的对外行为。

## 四、上游行为变化摘要

1. **parted 3.8**：3.7.13 修复 2 个 CVE（ChangeLog:99-101 核实为 libparted/fs/fat 内嵌 FAT 调整库堆溢出，PED_ASSERT 防御）——该库不在依赖面（SA2-014）；3.6.37 的 nilfs2 sigsegv 修复落在本轮重放行，nilfs2 未登记族（SA1-017）。无分区表面对外行为变化。
2. **util-linux 2.42.4**：ReleaseNotes 混入 nsenter 的 cgroup CVE（兄弟组件），libfdisk/分区面无行为变化。
3. **e2fsprogs 1.47.4**：本轮条目集中在 fuse2fs（FUSE 组件）、resize2fs extent 树/在线竞态、e2fsck 恢复逻辑、superblock 错误旗标——均为工具内部修复；本实现消费的契约（退出码 1/2/4、dumpe2fs Block count/Block size、尺寸回读恰好填满）未见变化。00295-00303 与 00001-00009 为 1.47.3/1.47.4 公告重复行。
4. **xfsprogs 7.3.0**：CHANGES 聚合块 9 条逐块核对均不含 xfs_growfs；xfs 面唯一依赖（growfs 的 data 几何回读，SA2-005）无变化。
5. **btrfs-progs 7.3**：新增条目为 corrupt-block/check/balance/receive 内部；文档面出现两条对实现有印证价值的表述——resize 次序（先分区后 fs / 先 fs 后分区，与 SA2-006/007/016 固定次序一致，doc-00034 认领）与 superblock total_bytes 作为占用事实源（doc-00035 认领）；低于最小值由内核拒绝、工具失败落拒绝继续（doc-00037 认领）。mkfs 选项/特性位大量新增（subvol/inode-flags/squota/raid-stripe-tree）均不在调用面。
6. **LVM2 2.03.44**：WHATS_NEW 条目集中在 VDO/thin/dmeventd/lvmlockd/raid/集群与 2.02.x 老版本聚合；lvresize -r 的 LV→crypt→FS 栈修复与 fsadm 装配属上游内部，等价契约仍由 lvextend -r 固定参数+回读证实承接（SA2-017）。
7. **ntfs-3g 2026.9.28**：ChangeLog 迁出 tarball，incidents 覆盖降为 0，已按 sources.tsv notes 登记为盲区（本三目录无该工具待判行）。

## 五、口径说明（供主控复核）

- T2 依赖工具（e2fsprogs/xfsprogs/btrfs-progs/lvm2）的 incidents 行延续既有批锚行口径：上游内部修复/特性一律裁剪（防线在调用面：退出码+回读证实+失败落已写待恢复，SA2-001/002/003 + SA2-011 版本固定）；四批（e2fsprogs/xfsprogs/btrfs/lvm2）的原锚行（00001）本轮被重放覆盖，已在重放行重新写全量锚 note，"同 xxx" 引用链已复原。
- docs 行按"描述已有行为→认领、绑定不存在特性→裁剪、排版行→裁剪"三分；btrfs resize 三行认领与既有行 00030/00072 同口径。
- implicit 行按"占用闸/覆写闸/固定步序已有等价→认领、交互应答类→裁剪"两分；lvm2 fsadm_cmd 装配行（00060/00061/00062）认领与既有锚行 00048 口径一致。
- 含新裁剪/拒绝行的 17 个批次 md 已在末尾追加"换版重放补充"小节（含撤回条件）；imp-e2fsprogs-prompt（纯认领）与 imp-xfsprogs-confirm（纯认领）不触发 md 更新。

## 六、批次文件清单

- 翻态 tsv（19 个）：`02-incidents/inc-{parted,libfdisk,e2fsprogs,xfsprogs,btrfs-progs,lvm2}.tsv`、`03-docs/docs-{btrfs-progs,lvm2}.tsv`、`04-implicit/imp-{libfdisk-confirm,e2fsprogs-confirm,e2fsprogs-default,e2fsprogs-prompt,xfsprogs-confirm,xfsprogs-default,btrfs-progs-confirm,btrfs-progs-default,btrfs-progs-lock,lvm2-confirm,lvm2-default}.tsv`
- 追加小节 md（17 个）：上述批次中含裁剪/拒绝行的同名 md。
