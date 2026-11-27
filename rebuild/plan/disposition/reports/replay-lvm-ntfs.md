# 换版重放处置报告：lvm2 2.03.16→2.03.44 / ntfs-3g 2022.10.3→2026.9.28

范围：`plan/disposition/05-sites-lvm2/` 与 `plan/disposition/05-sites-ntfs-3g/` 全部 state=待判 行（共 367 行，18 个批次）。行原文取自 `plan/baseline/reference/sites/lvm2.tsv`（2.03.44）与 `plan/baseline/reference/sites/ntfs-3g.tsv`（2026.9.28）。判断依据：`plan/disposition/PROTOCOL.md` 四态定义；`plan/baseline/incident-classes.tsv`（SA2-017、SA2-009/015、SA1-021、SA2-012）；`plan/baseline/refusal-map.tsv`（R0385-R0387、R0679-R0681）；`src/app/lvm.rs`、`src/app/execute/filesystem.rs`、`crates/core/src/fs/attrs.rs` 只读核对。queue.tsv 未改动（由主控统一重算）。

## 四态计数

| 批次 | 认领 | 拒绝 | 差异 | 裁剪 | 小计 |
| --- | ---: | ---: | ---: | ---: | ---: |
| sites-lvm2-frontend-error-report | 0 | 5 | 0 | 27 | 32 |
| sites-lvm2-frontend-fail-goto | 0 | 6 | 0 | 0 | 6 |
| sites-lvm2-frontend-refusal | 2 | 2 | 0 | 80 | 84 |
| sites-lvm2-lvm-error-report-lib | 14 | 17 | 0 | 36 | 67 |
| sites-lvm2-lvm-error-report-tools | 0 | 0 | 0 | 3 | 3 |
| sites-lvm2-lvm-fail-goto | 0 | 27 | 0 | 0 | 27 |
| sites-lvm2-lvm-refusal-lib | 19 | 6 | 0 | 74 | 99 |
| sites-lvm2-lvm-refusal-tools | 0 | 0 | 0 | 10 | 10 |
| sites-lvm2-resize-error-report | 0 | 0 | 0 | 4 | 4 |
| sites-lvm2-resize-refusal | 1 | 2 | 0 | 6 | 9 |
| **lvm2 小计** | **36** | **65** | **0** | **240** | **341** |
| sites-ntfs-3g-label-uuid-error-report | 0 | 0 | 0 | 1 | 1 |
| sites-ntfs-3g-label-uuid-refusal | 0 | 0 | 0 | 5 | 5 |
| sites-ntfs-3g-misc-error-report | 0 | 2 | 0 | 0 | 2 |
| sites-ntfs-3g-misc-refusal | 0 | 1 | 0 | 0 | 1 |
| sites-ntfs-3g-probe-error-report | 0 | 0 | 0 | 1 | 1 |
| sites-ntfs-3g-probe-fail-goto | 0 | 0 | 0 | 4 | 4 |
| sites-ntfs-3g-probe-refusal | 0 | 0 | 0 | 10 | 10 |
| sites-ntfs-3g-resize-fail-goto | 2 | 0 | 0 | 0 | 2 |
| **ntfs-3g 小计** | **2** | **3** | **0** | **21** | **26** |
| **总计** | **38** | **68** | **0** | **261** | **367** |

口径要点：

- lvm2 认领（36 行）：扩容 extent 算术与可分配余量（`pv_chain_step`，R0679/R0680/R0681）、pvresize 边界（最小尺寸/至少一个 extent）、活动性（`select_lv` Inactive）、lvextend -r 扩内层文件系统的前置/失败面、`grow_chain` 的 delta==0 与链后 `lv_state` 回读、重复 PV 设备/身份歧义（DeviceMatch 三态拒绝）、VG 读取一致性（seqno/缺失 PV/VG not found）——引 SA2-017 + `src/app/lvm.rs` 与 `src/app/decide/resize.rs` 代码路径。
- lvm2 拒绝（65 行）：全部为内部断言（INTERNAL_ERROR）与 fail-goto 清理跳转，标签架构差异-多虑。注意：换版重放新 fail-goto 行判**拒绝**（与 ntfs-3g lib 口径对齐），旧迁移 fail-goto 行（裁剪）保持不变、不回改；差异已在两批次 md 的"换版重放补充"小节登记。
- lvm2 裁剪（240 行）：vgchange/toollib/reporter/lvmcmdline 其余子命令与前端配置面；RAID/integrity/mirror/VDO/thin/cache/snapshot/stripe/crypt/pvmove 等未登记特性面；dm 激活/内核驱动面；缩容 fs 面（本实现 LVM 缩容只缩 PV 元数据，`shrink_pv`）；mda 维护/修复面；lvmlockd/cluster 锁面。
- ntfs-3g 认领（2 行）：ntfsresize `alloc_cluster` 簇分配失败面——SA2-009/015 固定参数消费链内交付（扩 -f -f / 缩 -f --size 绝对字节，真机锚 s18/s21）。
- ntfs-3g 拒绝（3 行）：libntfs-3g 内部错误通道（ntfs_log，logfile.c/attrib.c）——SA1-021 同族防御面口径，随消费工具链接库自带。
- ntfs-3g 裁剪（21 行）：ntfsinfo 探针面（不消费，识别以档案表为事实源）与 ntfslabel CLI 参数面（固定形式调用不可达，serial 由本实现生成后传入）。

## 待实现工作项

无。全部 38 行认领均已有归属，不产生新工作项：

- 判定层等价已在位：extent 算术与可分配余量（`src/app/decide/resize.rs::pv_chain_step`，R0679-R0681）、设备身份不可证明即拒绝（`src/app/lvm.rs::parse_pv_vg/parse_lvs_on_pv`）、活动性拒绝（`select_lv` → `LvmError::Inactive`）、delta==0 语义（`grow_chain`）、报表不可解析即拒绝（`report_parsable/report_rows`）；
- 链内交付已在位：pvresize 边界、lvextend -r 扩内层文件系统、ntfsresize 簇分配/坏簇重定位、并发守卫均随固定参数消费执行，硬失败经非零退出映射（R0386）。

若后续档案表把被裁剪面（vgchange/pvchange devices-file、RAID/VDO/thin、ntfsinfo 探针等）纳入登记，再按各批次 md"换版重放补充"小节的撤回条件重新逐行判断并立工作项。

## 差异建议

无新增差异行（差异 0）。旧差异 ntfs D15（标签 128 UTF-16 码元前置拒绝）不受本次换版影响，账本原状保留。

## 上游行为变化摘要（据账本行与代码站点）

- lvm2 2.03.44（相对 2.03.16）：
  - vgchange 新增/改写大量持久化预留（--persist/PR、lvmpersist）、locktype/setlockargs、systemid 面站点；
  - lvresize 内建文件系统处理通道（lib/fs：`_fs_reduce`/`_fs_extend`/crypt 分支）取代 fsadm 默认路径，fsadm 降级为 resize_fsadm 选项；
  - toollib 新增 writecache/integrity 设置校验、PR/persist 参数校验、devices file 自动导入（vgchange_activate）；
  - metadata.c 扩展 vg_validate（lock_args/历史 LV/重复 id）、`_wipe_outdated_pvs`、bad mda 修复告警；
  - pvresize/lvextend 核心校验（extent 上限、可分配余量、活动性、pvresize 最小尺寸）保持并小幅改写——均在消费链内交付。
- ntfs-3g 2026.9.28（相对 2022.10.3）：
  - ntfsinfo 扩展 $SDS/$SII 与索引分配转储站点（探针面，不消费）；
  - ntfslabel parse_options 改写、serial 十六进制校验（CLI 面，固定形式调用不可达）；
  - libntfs-3g attrib/logfile 防御面新增（ntfs_log 通道，判拒绝）；
  - ntfsresize `alloc_cluster` 改写（链内交付）。
- 盲区：两工具 ChangeLog 已迁出 tarball（ntfs-3g 指向 GitHub wiki，sources.tsv 已登记，incidents=0）；本摘要仅以参考账本站点为据，未补造 incidents 行。

## 产出文件

- 18 个批次 tsv 全部翻态，无残留待判；
- 15 个批次 md 末尾追加"换版重放补充"小节（含撤回条件；ntfs misc-error-report / misc-refusal / resize-fail-goto 三批新行口径与原 md 一致，未追加）；
- 本报告：`plan/disposition/reports/replay-lvm-ntfs.md`。
