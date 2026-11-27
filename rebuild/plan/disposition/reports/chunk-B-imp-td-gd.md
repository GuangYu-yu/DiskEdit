# 批次报告：chunk-B（04-implicit + 05-sites-testdisk + 05-sites-gdisk）

- 处理范围：`04-implicit/`（38 批，805 行）、`05-sites-testdisk/`（9 批，100 行）、`05-sites-gdisk/`（13 批，192 行）——共 **60 批次 / 1099 行**，全部翻态，无待判残留；未改 queue.tsv，未改任何代码。
- 参考账本：`plan/baseline/reference/implicit.tsv`（implicit_id）、`plan/baseline/reference/sites/{testdisk,gdisk}.tsv`（site_id）。

## 四态计数（全局 1099 行）

| 态 | 04-implicit | 05-sites-testdisk | 05-sites-gdisk | 合计 |
| --- | --- | --- | --- | --- |
| 认领 | 384 | 76 | 157 | **617** |
| 拒绝 | 14 | 0 | 26 | **40** |
| 差异 | 0 | 0 | 0 | **0** |
| 裁剪 | 407 | 25 | 10 | **442** |

含裁剪/差异面的 42 个批次均已创建同名 md（判断/依据/撤回条件）：04-implicit 23 份、05-sites-testdisk 9 份、05-sites-gdisk 10 份。

## 判断主轴（与 SA 账本的对齐关系）

- **sync/lock/kernel-reload（内核重载、落盘、独占）→ 认领**：统一引 SA1-011（写后 sync_all 错误上抛，`src/platform/linux/mod.rs::barrier`）、SA1-013（RefreshKernelView + BLKPG_RESIZE_PARTITION，失败落 5）、SA1-004/B5/B9（目标锁+占用闸+记录 O_EXCL）。例外：parted gnu.c 两行（Hurd 平台）与 Windows/多平台行 → 裁剪（Linux-only）；O_DIRECT 直通行 → 裁剪（本实现缓冲 I/O+回读）。
- **交互修复应答/confirm/force → 按等价物归类**：e2fsck -y/-n 语义类（imp-e2fsprogs-prompt 264 行）认领 SA2-001——档案表固定 `e2fsck -fp`（`src/app/execute/filesystem.rs:966`）+ 退出码 1/2=已修复 + 尺寸回读（SA2-003）；ntfsresize `-f` 认领 SA2-009（参数固定 -f 与绝对字节）；mkfs 类 force 覆写认领 SA1-005（覆写闸：`src/app/decide/new.rs::TableOccupied` + `--yes`）；sfdisk/btrfs 的 `--force 跳过全部一致性检查` 与共享锁写 → 拒绝（不设安全边界总旁路，SA1-005/SA1-004）。
- **gdisk 防御面 → 认领或拒绝（架构差异-多虑）**：真实 I/O/校验行认领（SA3-003/SA3-004、`crates/core/src/table/{mbr,gpt}`、`write.rs::WriteChannel` 回读）；内存分配失败 exit 与编译期类型尺寸断言（SizesOK）→ 拒绝，标签 `架构差异-多虑`（运行时 abort 与 Rust 布局承担，防御预算在唯一写点+回读）。
- **gdisk/testdisk 菜单类 → 裁剪**：cgdisk/gptcurses 菜单、逐字确认、备份文件、专家表搬移、盘标转换、分区交换、fixparts、分区类型列表、backup.log → 裁剪（引 SA1-020/SA3-002/SA1-016 或交互面裁剪，撤回条件见各 md）。
- **testdisk 恢复/重建类 → 对照 SA3-003/SA3-004 认领**：GPT 头/表校验、主备回退、条目不合法不猜、备份头重定位（SA1-018）、MBR 结构/扩展链/重叠校验（`crates/core/src/table/mbr`）；内容级识别（RAID/LVM/HFS/APFS/BFS/FAT boot）→ 裁剪（SA1-017）；老盘标（humax/mac/sun/xbox/bsd）→ 裁剪（SA1-016）；MBR 引导代码写入 → 裁剪（写面=表结构，SA1-027）。
- **#define 常数群（default 批次）→ 二分**：表结构常数（0xAA55/0xEE/128 条目/1MiB 对齐/CHS 占位/Linux 类型 0x83）认领并锚到 `crates/core/src/table/*`；参考工具内部常数（fs 布局、进度宏、选项表、LZ4、I/O 缓存、lvm2/fsadm 内部）→ 裁剪（TOOLFACE：能力只在档案表登记，SA1-017/SA2 系）。

## 待实现工作项（1 项）

```
testdisk-00012 | GPT 条目数组字节总量的显式下限校验（UEFI §5.3.1.2：至少 16384B；读侧拒绝 <16384B 的自洽表） | 缺校验(签名/边界/容量) | parse 层已校验条目数（>0）与条目尺寸（≥128B），未见数组字节总量的 16384B 下限断言（创建路径固定 128×128B=16KiB） | 门控+测试：crates/core/src/table/gpt/parse.rs 对 entry_array_bytes<16384 返回 InvalidGeometry 并落用例（锚 tests/gpt_repair.rs）
```

## 差异建议（无差异态行；以下为「本实现更保守」的既定差异，供主控决定是否登记 D 编号）

1. **D-建议（gdisk-00102..00105 / gdisk-00182..00184）**：参考工具对条目尺寸不符自适应改写、对越界 LBA 钳制继续；本实现严格校验拒绝（不静默改写/不落越界条目）。已按 SA3-004 读侧校验原则与 SA1-014（对齐下取整为唯一合法化路径）登记在行 note，可不另立 D。
2. **D-建议（testdisk-00008/00009）**：头 rev/reserved 字段参考工具"警告继续"，本实现按损坏走 SA3-003 矩阵（严格拒绝/修复）。若用户侧需要告警可见性，建议把头字段警告加进 inspect 诊断输出（提示面缺失标签的小工作项），不改变写侧严格性。
3. **D-建议（gdisk-00084）**：gdisk 在读扫描中就地自动修 PMBR；本实现只读不改（SA1-002/SA1-003），PMBR 重写只并入表写。属已认领行为（SA1-003），无需行动。

## 拆分建议

- **无强制拆分**：各批内同结论行已用「同上」折叠，批内混合结论均已逐行标注。
- 可选拆分 1：`imp-parted-default`（71 行）混有表结构常数（认领）与老盘标/fs 布局常数（裁剪）两类根因；若主控要按根因标签重算，可拆为 `imp-parted-default-consts`（认领）/`imp-parted-default-legacy`（裁剪）。
- 可选拆分 2：`imp-e2fsprogs-prompt`（264 行）为单一结论（SA2-001 承接），如需按问题码族（PROMPT_FIX/RELOCATE/CONNECT…）细化再评估，现阶段不建议拆。

## 拒绝面摘要（40 行，全部有取舍理由）

- `--force 跳过全部一致性检查` 总旁路（libfdisk 3 行）：SA1-005 只解锁覆写不关校验。
- 挂载卷 `--force` 检查旁路（btrfs 9 行）：SA1-004 占用闸一律拒绝。
- 共享锁写（gdisk-00043）：绕过占用闸，写必须独占。
- 静默删除/自动纠正（gdisk-00024 删越界分区、gdisk-00022 exit 风格）：不静默改写用户数据（SA3-001 同原则）、退出码单源（SA1-009）。
- 分配失败 exit/编译期断言（gdisk 22 行）：`架构差异-多虑`，运行时与语言布局承担。

## 产物清单

- 60 个批次 tsv（全部翻态）+ 42 个同名 md（撤回条件）。
- 本报告：`plan/disposition/reports/chunk-B-imp-td-gd.md`。
- 未触碰：`queue.tsv`、`crates/`、`src/`、`tests/`。
