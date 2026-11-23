# 能力矩阵取数（T0 冻结）

本表是 `03-行为基线.md` 第 4 节的来源。每格给出结论与源码位置；结论只有「是 / 否 / 不支持 / 设计限制」，没有待确认项。源码位置以仓库根（`rebuild/` 之外，只读）为准。

## 1. 结论表

| 族 | 扩容 | 收缩 | 属性（标签 / UUID） | 检查 | 建卷 |
| --- | --- | --- | --- | --- | --- |
| ext | 是 | 是 | 是 | 是 | 是 |
| xfs | 是（需挂载） | 否（设计限制） | 是 | 是 | 是 |
| btrfs | 是 | 单设备可；多设备否（设计限制） | 是 | 是 | 是 |
| f2fs | 是 | 否（设计限制） | 否（设计限制） | 是 | 是 |
| ntfs | 是 | 是 | 是 | 是 | 是 |
| vfat | 是 | 否（设计限制） | 是 | 是 | 是 |
| exfat | 否（设计限制） | 否（设计限制） | 是 | 是 | 是 |
| swap | 否（不支持） | 否（不支持） | 是 | 否（不支持） | 是 |
| 其余识别族 | 否 | 否 | 否 | 否 | 否 |

## 2. 逐格来源

| 格 | 源码位置 | 判定 |
| --- | --- | --- |
| ext 扩容 / 收缩 | `src/fsops/capabilities.rs` `grow_support` / `shrink_support` 的 `Filesystem(FsKind::Ext)` 臂 | `Tools(EXT_TOOLS)`，是 |
| xfs 扩容 | 同文件 `Filesystem(FsKind::Xfs)` 臂 | `Tools(&[XfsGrowfs])`，是；`mount_layer_needs` 对 xfs 合并挂载与卸载，故需挂载 |
| xfs 收缩 | `shrink_support` 无 `Xfs` 臂，落到 `_ =>` | `Unsupported(…not wired to a tool…)`，设计限制 |
| btrfs 扩容 / 收缩 | `grow_support` / `shrink_support` 的 `Btrfs` 臂 | `Tools(BTRFS_TOOLS)`，是；`shrink_scope` 由 `src/fsops/scope.rs` 的多设备判定收口 |
| f2fs 扩容 | `grow_support` 的 `F2fs` 臂 | `Tools(&[FsckF2fs, ResizeF2fs])`，是 |
| f2fs 收缩 | `shrink_support` 无 `F2fs` 臂，落到 `_ =>` | 设计限制 |
| f2fs 属性 | `src/fsops/ext.rs` `Label::for_fstype` / `UuidSet::for_request` / `UuidRead::for_fstype` 均无 `F2fs` 臂 | 无工具，设计限制 |
| f2fs 检查 | `src/fsops/ext.rs` `Check::for_fstype` 的 `F2fs` 臂 | `Check::F2fs`（`FsckF2fs`），是 |
| f2fs 建卷 | `src/fsops/mkfs.rs` `mkfs_tool` 的 `MkfsFs::F2fs` 臂 | `MkfsF2fs`（带 `-f`），是 |
| ntfs 扩容 / 收缩 | `grow_support` / `shrink_support` 的 `Ntfs` 臂 | `Tools(NTFS_TOOLS)`，是 |
| ntfs 属性 | `Label::for_fstype` 的 `Ntfs` 臂；`UuidSupport::GeneratedOnly`（`uuid_support`） | 标签与 UUID 均为是；UUID 只可生成新值 |
| ntfs 检查 | `Check::for_fstype` 的 `Ntfs` 臂 | `Check::Ntfs`（`Ntfsfix -n`），是 |
| ntfs 建卷 | `mkfs_tool` 的 `MkfsFs::Ntfs` 臂 | `MkfsNtfs`，是 |
| vfat 扩容 | `grow_support` 的 `Vfat` 臂 | `Tools(&[Fatresize])`，是 |
| vfat 收缩 | `shrink_support` 无 `Vfat` 臂，落到 `_ =>` | 设计限制 |
| vfat 属性 | `Label::for_fstype` 的 `Vfat` 臂 | `Fatlabel`，是；UUID 无工具，按标签单列 |
| vfat 检查 | `Check::for_fstype` 的 `Vfat` 臂 | `Check::Vfat`，是 |
| vfat 建卷 | `mkfs_tool` 的 `MkfsFs::Vfat` 臂 | `MkfsVfat`，是 |
| exfat 扩容 | `grow_support` 无 `Exfat` 臂，落到 `Filesystem(_) => Unsupported` | 设计限制 |
| exfat 收缩 | `shrink_support` 无 `Exfat` 臂，落到 `_ =>` | 设计限制 |
| exfat 属性 | `Label::for_fstype` 的 `Exfat` 臂 | `Exfatlabel`，是 |
| exfat 检查 | `Check::for_fstype` 的 `Exfat` 臂 | `Check::Exfat`，是 |
| exfat 建卷 | `mkfs_tool` 的 `MkfsFs::Exfat` 臂 | `MkfsExfat`，是 |
| swap 扩容 | `grow_support` 的 `Swap` 臂 | `Tools(&[Mkswap])`；没有原地扩容，靠重建吸收（`shrink_support` 明确拒绝） |
| swap 收缩 | `shrink_support` 的 `Swap` 臂 | 无此臂，`_ =>` 设计限制；`shrink` 属重建 |
| swap 属性 | `Label::for_fstype` 的 `Swap` 臂 | `Swaplabel`，是 |
| swap 检查 | `Check::for_fstype` 无 `Swap` 臂 | 否 |
| swap 建卷 | `mkfs_tool` 的 `MkfsFs::Swap` 臂 | `Mkswap`，是 |
| LVM PV 建卷 | `mkfs_tool` 的 `MkfsFs::Lvm2Pv` 臂 | 拒绝并指路，`06` |
| 其余识别族 | `grow_support` / `shrink_support` / `Label` / `Check` / `mkfs_tool` 的兜底臂 | 均为不支持或设计限制 |

## 3. 停工核对

- 7 处原「待确认」已逐格读出：f2fs 收缩 / 属性 / 建卷、vfat 收缩、exfat 扩容 / 收缩 / 建卷、ntfs 建卷。
- 没有读不出的格；因此 T0 未触发 `01` 第 8 节停工条件 1。
- 本表只记录源码结论；实跑与掉电持久性不在 T0 范围。
