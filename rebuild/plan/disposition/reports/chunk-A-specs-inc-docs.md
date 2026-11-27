# chunk-A 报告：01-specs + 02-incidents + 03-docs（21 批，1057 行）

判定人：chunk-A 处置轮。逐行 state+note 见各批次 tsv；含裁剪/差异的批次均有同名 md（判断/依据/撤回条件）。本报告只汇总可执行结论。

## 一、待实现工作项（1 项）

```
EBR-003|沿链解析逐级校验 EBR 的 0x55AA 签名，缺签名即拒绝继续沿链|specs-microsoft EBR|缺校验(签名/边界/容量)|硬约束：walk_extended 逐级读回 512B 后先验字节签名再解析链接/数据记录（现 mbr/extended.rs 只按链接记录推进）；测试锚：tests/case_runner.rs 构造缺签名 EBR 的 MBR 镜像断言沿链拒绝，观察层报 InvalidGeometry/按失败处理
```

其余全部为"已有行为"或"裁剪/拒绝"，无新增待实现面。

## 二、差异建议（2 项，均已入行 note 并建 md）

1. `GPT-011`（specs-uefi）：PMBR 重写保留 0..446（磁盘签名+引导代码），不按 UEFI §5.2.3 清零 Disk Signature；新建空盘时自然为零。依据 `create.rs::protective_mbr_image`+基线等价测试。撤回条件：出现强制签名为零的外部契约或基线行为变更。
2. `gdisk-inc-00002`：小数尺寸按精确字节分数换算（9.5G=9.5GiB 再对齐，`src/app/cli/parse.rs::absolute_size`），不复刻 gdisk 的整数截断；9 扇区灾难误读不存在。撤回条件：逐字节对表需复刻截断语义时加取整模式。

## 三、拆分建议（均在行 note 标注，不强拆）

1. `03-docs/docs-btrfs-progs`：resize 认领行（00028-00036/00062/00094/00104/00018-00020）与 123 条上游选项/显示面裁剪行混批 → 可拆 `docs-btrfs-resize` 与 `docs-btrfs-misc`（00028/00080 已标注）。
2. `02-incidents/inc-parted`：00011（按文件系统名映射类型码）独立面 → `inc-parted-type-map`。
3. `02-incidents/inc-gdisk`：00040（Apple 类型 GUID 展示名映射）→ `inc-gdisk-type-names`。
4. `02-incidents/inc-libfdisk`：40/41 行为 util-linux 其他组件公告，若后续开"依赖工具面"专用批可整体移交（当前已按裁剪处置，无阻塞）。

## 四、四态计数

| 批次 | 认领 | 拒绝 | 差异 | 裁剪 | 小计 |
|---|---|---|---|---|---|
| 01-specs/specs-uefi | 22 | 0 | 1 | 0 | 23 |
| 01-specs/specs-microsoft | 6 | 0 | 0 | 0 | 6 |
| 02-incidents/inc-parted | 9 | 8 | 0 | 5 | 22 |
| 02-incidents/inc-libfdisk | 0 | 1 | 0 | 40 | 41 |
| 02-incidents/inc-gdisk | 8 | 12 | 1 | 19 | 40 |
| 02-incidents/inc-e2fsprogs | 1 | 0 | 0 | 274 | 275 |
| 02-incidents/inc-xfsprogs | 0 | 0 | 0 | 93 | 93 |
| 02-incidents/inc-btrfs-progs | 0 | 0 | 0 | 67 | 67 |
| 02-incidents/inc-dosfstools | 0 | 0 | 0 | 64 | 64 |
| 02-incidents/inc-lvm2 | 1 | 0 | 0 | 86 | 87 |
| 02-incidents/inc-testdisk | 0 | 0 | 0 | 61 | 61 |
| 03-docs/docs-parted | 13 | 0 | 0 | 13 | 26 |
| 03-docs/docs-libfdisk | 2 | 0 | 0 | 0 | 2 |
| 03-docs/docs-gdisk | 0 | 0 | 0 | 38 | 38 |
| 03-docs/docs-e2fsprogs | 1 | 0 | 0 | 16 | 17 |
| 03-docs/docs-xfsprogs | 1 | 0 | 0 | 1 | 2 |
| 03-docs/docs-btrfs-progs | 15 | 0 | 0 | 123 | 138 |
| 03-docs/docs-ntfs-3g | 4 | 0 | 0 | 13 | 17 |
| 03-docs/docs-f2fs-tools | 0 | 0 | 0 | 1 | 1 |
| 03-docs/docs-dosfstools | 0 | 0 | 0 | 4 | 4 |
| 03-docs/docs-lvm2 | 1 | 0 | 0 | 32 | 33 |
| **合计** | **84** | **21** | **2** | **950** | **1057** |

判定口径：依赖工具（e2fsprogs/xfsprogs/btrfs/dosfstools/lvm2/testdisk）行一律按"依赖工具面"裁剪（本实现只按档案表调用+退出码/回读承接，SA2-001~017）；纯 C 内存/平台崩溃修复判拒绝（防御由 Rust 所有权/类型承担）；交互 TUI/混合 MBR 写面/zap/备份文件恢复/CHS 换算/老盘标按对应 SA 裁剪。
