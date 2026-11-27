# chunk-G：lvm2 + ntfs-3g 站点账本处置报告

范围：`plan/disposition/05-sites-lvm2/`（14 批）+ `plan/disposition/05-sites-ntfs-3g/`（18 批），共 32 个批次 tsv、1861 行（行原文：`plan/baseline/reference/sites/lvm2.tsv` 1082 行含表头、`ntfs-3g.tsv` 781 行含表头）。

## 四态计数

| 工具 | 认领 | 拒绝 | 差异 | 裁剪 | 合计 |
| --- | --- | --- | --- | --- | --- |
| lvm2 | 44 | 113 | 0 | 924 | 1081 |
| ntfs-3g | 56 | 554 | 1 | 169 | 780 |
| **合计** | **100** | **667** | **1** | **1093** | **1861** |

## 总体判断

- **lvm2**：本实现只消费 `--grow-lv` 次序（写表→pvresize→lvextend -r，SA2-017 已定案；消费面 `src/app/lvm.rs::grow_chain/select_lv/pv_size_report`、`src/app/decide/resize.rs::pv_chain_step`，refusal-map R0385-R0387、R0679-R0681）。据此：
  - PV/VG/LV 尺寸校验（_reduce_pv/_extend_pv/_pv_resize/MAX_EXTENT_COUNT/新尺寸 0 或与现尺寸相同）、pvresize 边界（min size/pe start/设备小于 PV/PV 超设备）、LV 活动性、lvextend -r 的 fsadm 失败、metadata 一致性告警 → **认领引 SA2-017**（等价拒绝在判定层或由链内工具交付+非零退出映射 R0386）；
  - 其余子命令（pvchange/vgchange/reporter/通用 lvm CLI）、lvm 库内部错误通道（lib/metadata 分配/段/镜像/快照/改名/删除/锁定）、设备 IO 层（lib/device/dev-io.c）、全部 fail-goto 清理路径 → **裁剪**（范围依据：档案表只登记 grow-lv 链）；
  - INTERNAL_ERROR 内部断言 → **拒绝（架构差异-多虑）**。
- **ntfs-3g**：本实现只消费 ntfsresize（SA2-009/015 已定案；扩 `-f -f`、缩 `-f --size <bytes>`，`src/app/execute/filesystem.rs::grow_plan/shrink`）与 ntfsfix -n 只读检查、ntfslabel 标签/serial 面。据此：
  - ntfsresize 的尺寸校验、卷回读、坏簇重定位提示、挂载卷检查、写回（bootsector/bitmap/MFTMirr/$Boot）失败 → **认领引 SA2-009/015**（挂载检查由占用闸 B4/B17 写前等价前置）；
  - ntfsfix 修复面（只消费 -n 只读检查）、ntfsinfo/ntfscluster 探针面、ntfsresize CLI/usage/交互确认/no-action 面、ntfslabel CLI 与显式 serial 解析面（本实现 serial 仅生成，`attrs.rs UuidSupport::GeneratedOnly`）→ **裁剪**；
  - libntfs-3g 内部错误通道（ntfs_log）防御类 → **拒绝（架构差异-多虑，SA1-021 同族防御面口径）**——不复制库内通道，工具原文输出可见（D3），硬失败经退出码映射兜住。

## 待实现工作项

| row_id | 行为 | 标签 | 根因 | 建议硬约束 |
| --- | --- | --- | --- | --- |
| ntfs-3g-00063 | NTFS 收缩后用 `ntfsresize --info` 回读卷尺寸，只接受缩到目标（尺寸不大于请求值）的回读，不足落已写待恢复 | 缺回读证实 | 产品收缩路径（`src/app/execute/filesystem.rs::shrink`，FsKind::Ntfs）只信 ntfsresize 退出码；SA2-015 的"回读下降"语义目前只由 qemu s21 测试锚承担，而 ext 已有 verify_ext_size 对称闸、btrfs/xfs 有各自回读闸，NTFS 是唯一无产品侧回读的可收缩族 | 测试：收缩用例断言回读不足时落已写待恢复（仿 `the_size_readback_accepts_only_a_filled_filesystem`）；仪器：filesystem.rs 为 FsKind::Ntfs 增加对称回读闸（parse `Current volume size`，字段缺失即拒绝不猜默认值，与 SA2-011 同口径） |

## 差异建议（交主控统一登记 D 编号）

1. `ntfs-3g-00077`（ntfslabel change_label，label-uuid-refusal）：NTFS 卷标超长（>128 UTF-16 码元）时参考工具截断+警告后照常设置，本实现在写任何字节前按字段容量拒绝。`D-建议:NTFS 卷标超长按 128 UTF-16 码元字段容量前置拒绝，不采纳截断`——依据 `crates/core/src/fs/attrs.rs::label_problem`（129 码元拒绝单测同族）。撤回条件：出现依赖"超长截断"语义的外部契约或要求与基线逐字节等价改标。

## 观察（移交主控，不属本批 ledger 行）

- lvm 链已有回读闸：`src/app/lvm.rs::grow_chain` 在 lvextend 报成功后回读 LV 尺寸（"reported success, but … is … bytes (expected at least …)" 落 Postcondition），PV 侧 `pv_size_report` 严格解析——lvm2 无回读缺口，故本 chunk 唯一工作项在 NTFS。
- ntfsresize/ntfslabel 的挂载卷拒绝由占用闸写前等价前置（B4/B17），链内检查随消费工具自带；若未来把"check 形态 ntfsfix -n 对已挂载卷"放开为可执行，需复核 label/resize 的挂载行认领口径。

## 拆分建议

混合结论批次的少数派行已在行 note 标 `建议拆分`（不强拆）：

- `sites-lvm2-resize-refusal`：1 认领（lvm2-00018）+ 5 裁剪 + 1 拒绝 → 建议新批 `sites-lvm2-resize-lv-selection`；
- `sites-lvm2-lvm-error-report-lib`：31 认领 + 63 拒绝 + 175 裁剪 → 认领行建议新批 `sites-lvm2-lvm-chain-error-report`；
- `sites-lvm2-lvm-refusal-lib`：12 认领 + 34 拒绝 + 270 裁剪 → 认领行建议新批 `sites-lvm2-lvm-chain-refusal`；
- `sites-ntfs-3g-resize-fatal-exit` / `sites-ntfs-3g-resize-string-channel`：20/31 认领 vs 10/4 裁剪 → 裁剪行建议新批 `sites-ntfs-3g-resize-cli-surface`；
- `sites-ntfs-3g-label-uuid-refusal`：2 认领 + 3 裁剪 + 1 差异 → 裁剪行建议新批 `sites-ntfs-3g-label-uuid-cli-surface`。

## 逐批明细

### lvm2（1081 行）

| 批次 | 行数 | 结论 |
| --- | --- | --- |
| sites-lvm2-resize-refusal | 7 | pvresize/lvresize CLI 拒绝面裁剪；INTERNAL_ERROR 拒绝；LV 未找到认领（select_lv/R0386/R0387） |
| sites-lvm2-resize-fail-goto | 4 | pvresize/lvresize 内部清理路径裁剪 |
| sites-lvm2-resize-error-report | 9 | lvresize CLI 参数面裁剪（固定参数 lvextend -l +<N> -r 不可达） |
| sites-lvm2-frontend-refusal | 192 | 其余子命令（pvchange/vgchange/toollib/reporter）裁剪；INTERNAL_ERROR 拒绝 |
| sites-lvm2-frontend-error-report | 100 | 其余子命令文案面裁剪；INTERNAL_ERROR 拒绝 |
| sites-lvm2-frontend-fail-goto | 33 | 其余子命令清理路径裁剪 |
| sites-lvm2-frontend-fatal-exit | 3 | become_daemon 后台化出口裁剪 |
| sites-lvm2-lvm-refusal-lib | 316 | 库内部路径裁剪；PV/VG/LV 尺寸校验与 pvresize 边界 12 行认领引 SA2-017；INTERNAL_ERROR 拒绝 |
| sites-lvm2-lvm-error-report-lib | 269 | 库内部路径裁剪；尺寸校验/边界/一致性/活动性 31 行认领引 SA2-017；INTERNAL_ERROR 拒绝 |
| sites-lvm2-lvm-refusal-tools | 38 | 通用 lvm CLI 参数面裁剪；INTERNAL_ERROR 拒绝 |
| sites-lvm2-lvm-error-report-tools | 52 | 通用 lvm CLI 错误面裁剪 |
| sites-lvm2-lvm-fail-goto | 51 | CLI 与库内部失败清理裁剪 |
| sites-lvm2-misc-refusal | 4 | 设备 IO 层（dev-io.c）裁剪 |
| sites-lvm2-misc-error-report | 3 | 设备 IO 层裁剪 |

### ntfs-3g（780 行）

| 批次 | 行数 | 结论 |
| --- | --- | --- |
| sites-ntfs-3g-resize-fatal-exit | 30 | 尺寸校验/卷回读/坏簇重定位/挂载检查认领引 SA2-009/015；usage/助手出口/no-action 面裁剪 |
| sites-ntfs-3g-resize-string-channel | 35 | 卷回读/坏簇提示/尺寸约束/写回失败认领；version/unknown option/no-action 面裁剪 |
| sites-ntfs-3g-resize-fail-goto | 2 | 位图核对/重定位清理认领引 SA2-009/015 |
| sites-ntfs-3g-label-uuid-refusal | 6 | CLI/显式 serial 解析面裁剪；挂载拒绝认领（占用闸）；超长标签登记差异 |
| sites-ntfs-3g-label-uuid-error-report | 3 | CLI 面裁剪；标签编码校验认领（label_problem） |
| sites-ntfs-3g-label-uuid-fatal-exit | 2 | usage/出口裁剪 |
| sites-ntfs-3g-frontend-refusal | 54 | ntfsfix 修复面裁剪（只消费 ntfsfix -n 检查）/ntfscluster 不消费 |
| sites-ntfs-3g-frontend-error-report | 7 | 同上 |
| sites-ntfs-3g-frontend-fail-goto | 28 | ntfsfix 修复路径清理裁剪 |
| sites-ntfs-3g-frontend-fatal-exit | 14 | usage/出口裁剪 |
| sites-ntfs-3g-probe-refusal | 22 | ntfsinfo 探针面裁剪 |
| sites-ntfs-3g-probe-error-report | 7 | 同上 |
| sites-ntfs-3g-probe-fail-goto | 2 | 同上 |
| sites-ntfs-3g-probe-fatal-exit | 4 | 同上 |
| sites-ntfs-3g-probe-string-channel | 10 | 同上 |
| sites-ntfs-3g-misc-refusal | 234 | libntfs-3g 内部错误通道拒绝（架构差异-多虑，SA1-021 同族口径） |
| sites-ntfs-3g-misc-fail-goto | 195 | 同上 |
| sites-ntfs-3g-misc-error-report | 125 | 同上 |

每批含裁剪/差异行，均已创建同名 md（判断/依据/撤回条件）。queue.tsv 未改动。
