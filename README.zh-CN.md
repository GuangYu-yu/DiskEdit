# DiskEdit

把磁盘与磁盘镜像统一当作可按偏移读写的字节存储，在其上编辑分区表（GPT/MBR）、操作文件系统、搬移分区。

[English](README.md) | 简体中文

- **镜像与块设备走同一条代码路径**：`.img` 与 `/dev/sdX` 的差别只在目标的打开方式。
- **GPT/MBR 由本工具自己解析与写入**，不经过 libparted 或 sfdisk。GPT 提交依次写备条目数组、备头、主条目数组、主头，每步 `sync`，任意落点断电都至少留一份自洽副本，不一致由 CRC 检出。块设备上分区处于挂载状态时内核重读必然失败，该路径改用 `sfdisk --no-reread` 写表，再由 `partx -u` 同步内核。
- **文件系统交给各自的官方工具**：mkfs、fsck、resize、label、UUID 都在 offset loop（`losetup`）上执行。
- **长作业可续跑**：搬移、扩容或 mkfs 中途中断后，重跑同一条命令即按恢复记录续跑。`inspect` 与 `records` 报告现场，`recover` 回滚已记下的写入，`recover --release` 放弃现场。
- **输出可被脚本读**：`info`、`records list|verify|scan` 输出 JSON；`info --grow` 把搬移预览并进同一份报告。

## 运行要求

- **Linux**。只有 Linux 承诺写盘；其他平台在命令分派处按缺失的原语拒绝——逐条答案见 [docs/zh/capability-matrix.md](docs/zh/capability-matrix.md)。
- **Rust**：需要支持 edition 2024 的工具链（1.85 及以上）才能构建。
- **root**：文件系统层命令需要，因为要经 `losetup` 映射分区。分区表写入只对块设备需要 root，对镜像文件不需要。

| 操作 | 镜像文件 | 块设备 |
|---|---|---|
| `info` / `inspect` / `records` | 不需要 root | 不需要 root |
| 分区表写入（`new`/`add`/`delete`/`resize`/`copy`/`set`/`recover`/`move`） | 不需要 root | 需要 root |
| 文件系统层（`mkfs`/`resizefs`/`check`/`set label\|uuid`） | 需要 root | 需要 root |

`resize` 在含文件系统步骤时需要 root；`--no-fs` 表示不把文件系统增长算作后置条件，镜像上的扩容因此无需 root。目标文件系统无法识别（`unknown`）时，不带 `--no-fs` 一律拒绝——“认不出”不等于“里面没有东西”。

外部工具按需查找，只在该步骤用到时经 `PATH` 调用：`sfdisk`、`partx`、`losetup`、`mount`，以及各文件系统自己的工具（`mke2fs`/`e2fsck`/`resize2fs`、`mkfs.xfs`/`xfs_growfs` 等）。完整清单见 [docs/zh/internals.md](docs/zh/internals.md#外部工具)。

写命令直接修改目标本身，改错后原有内容可能无法恢复。原地修改的好处是不需要第二份镜像的空间，也不会留下需要清理的额外文件；数据重要时，请在执行写命令前先备份。

## 构建

```bash
cargo build --release
```

产物为 `target/release/DiskEdit`（帮助与消息里以 `diskedit` 称呼它）。

## 快速上手

```bash
truncate -s 8G disk.img
diskedit new disk.img --table gpt --yes
diskedit add disk.img 4G --yes
sudo diskedit mkfs disk.img:1 ext4 --yes
diskedit info disk.img | jq .
```

`diskedit help` 给出的示例：

```bash
diskedit info /dev/sdb
diskedit info /dev/sdb:2 --grow
diskedit resize /dev/sdb:1 +2G --yes
diskedit resize disk.img grow --allow-move --chunk 4 --yes
diskedit recover /dev/sdb --yes
```

`<TARGET>` 为镜像路径或块设备；`:N` 寻址第 N 个分区（1 起），`:last` 指最后一个可操作分区。`diskedit help <CMD>` 查看单命令详助。

`SIZE` 接受绝对值（`10G`）、增量（`+2G`）、百分比（`+10%`）与关键字 `grow`。单位按 1024 进制；不落在扇区边界上的尺寸会被截断，并报告截断。

### 原地重排分区

目标被原地编辑，不生成新的整盘副本。重排由若干独立命令组成，每条命令各自确认、各自留记录；中途失败时，已完成的步骤保持已应用状态，未完成的可沿记录续跑。

- **先扩容器，再动分区。** raw 镜像的容量由文件本身决定：先用 `truncate -s 20G disk.img` 把文件放大，再调整其中的分区。扩展后的 GPT 备份头仍停在旧末端；下一条写表命令会把它随计划一起搬到新末端。
- **增长。** `diskedit resize disk.img:1 +2G --yes`；`resize ... grow` 吃满右侧连续空闲，`--grow-to-end` 把末端钉在 `last_usable_lba`。挡路的分区只有给出 `--allow-move` 才会搬移，且先打印计划。
- **收缩。** `diskedit resize disk.img:1 -500M --yes` 先离线缩文件系统、再收分区条目，GPT 与 MBR 主分区都支持。
- **搬移。** `diskedit move disk.img:2 --start <LBA> --yes` 把分区与数据一起搬到新起点，可沿其记录续跑。MBR 没有搬移能力。
- **把 A 的空间交给 B**（仅 GPT——MBR 没有搬移能力，空洞无法转交）：先缩 A 腾出空洞，把 B 搬进去，再让 B 向右吃满它腾出的位置：

  ```bash
  diskedit resize disk.img:1 -2G --yes
  diskedit move disk.img:2 --start <紧接 A 之后的 LBA> --yes
  diskedit resize disk.img:2 grow --yes
  ```

- **`--delete`。** `diskedit delete disk.img:2 --yes` 删掉条目，其后的分区留在原位；空洞用上面的配方回收。命令没有点到的分区，数据与位置都保持原样。
- **追加空分区。** `diskedit add disk.img <SIZE> --yes` 追加条目；`resize` 从不创建条目。
- **预览。** `--dry-run` 打印区间、文件系统步骤与可能的搬移计划，不写盘，也不需要 `--yes`。
- **起点对齐与 NTFS 引导扇区。** `move`（或因搬移而改变起点的 `resize`）把 NTFS 分区换到新起始 LBA 时，会把 VBR 偏移 0x1c 处的 hidden sectors 字段改写成新 LBA；起点超出 32 位字段时会给出警告。`resize` 保持原起点；`move :1 --start align` 把分区放到下一个 1 MiB 边界。右侧有邻居挡路时按范围重叠拒绝，不会替用户搬移邻居；MBR 分区没有搬移能力。
- **没有强制开关。** 文件系统无法报告出小于请求的尺寸时，收缩会在任何写表之前被拒绝（ext 的预查用 `resize2fs -P` 取最小尺寸）。

## 支持的文件系统

识别（`info`）：ext、xfs、btrfs、f2fs、ntfs、vfat、exfat、swap、squashfs、erofs、hfsplus、apfs、lvm2_pv、luks、bitlocker、bcachefs、jfs、reiserfs、udf；都不匹配则为 `unknown`。后六种只做识别，`mkfs`/`check`/`resize` 均未接线（bcachefs 可跨多块设备，成员列表也不读取）。

| FS | mkfs | 扩容 | 缩容 | check | label | uuid |
|---|---|---|---|---|---|---|
| ext2/3/4 | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| xfs | ✓ | ✓ | — | ✓ | ✓ | ✓ |
| btrfs | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| ntfs | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| f2fs | ✓ | ✓ | — | ✓ | — | — |
| vfat | ✓ | ✓ | — | ✓ | ✓ | — |
| exfat | ✓ | — | — | ✓ | ✓ | — |
| swap | ✓ | ✓ | — | — | ✓ | ✓ |
| lvm2_pv | — | ✓ | ✓ | — | — | — |

- `check` 一律只读（`e2fsck -n`、`ntfsfix -n`、`xfs_repair -n`、`btrfs check`、`fsck.vfat -n`、`fsck.exfat -n`、`fsck.f2fs`）；工具退出非零即检查失败（退出码 3）。
- `resize` 按尺寸表达式扩缩 GPT 或 MBR 主分区（`+2G`、`-500M`、`10G`、`grow`）；收缩先离线跑文件系统步。MBR 覆盖 1..4，扩展容器与逻辑分区不在此列。`resize --dry-run` 只打印计划，不写盘。`resizefs` 只扩，在线 btrfs 形式也能缩。文件系统缩不动就不动表。
- `mkfs` 拒绝 LVM PV，重建请用 `pvcreate(8)` / `wipefs(8)`。
- `ntfs` 的 UUID 只能重新生成（`ntfslabel --new-serial`），不能指定；该序号不是 Windows 卷 UUID。
- `swap` 的扩容是改完表项后用 `mkswap` 重建，UUID、PARTUUID 与分区号保持。
- `lvm2_pv` 的扩容只到 PV 层；`resize --grow-lv` 才继续扩 LV 及其文件系统。镜像目标上的缩容在改表项之前先跑 `pvresize --setphysicalvolumesize`（块设备拒绝缩容），要求新末端之后没有已分配 extent（先用 `pvmove` 挪走），且 VG 不处于活动状态。临时 loop 可能让 udev 重新激活 VG，需同时 `lvchange -ky <vg/lv>`；`pvresize` 之后若出现持有者，表项保持不变。
- OpenWrt combined 布局（同一分区内只读 squashfs/erofs 根 + 尾部 RW overlay）只扩 RW 层，且内层文件系统须为 ext 或 f2fs。

## 输出与退出码

`info` 与 `records list|verify|scan` 恒在 stdout 输出一份 JSON；失败时信封里加一个 `error` 字符串，无法确定的字段为 `null`。文档完整之前 stdout 不输出任何内容，读取方可以直接解析。

| 退出码 | 含义 |
|---|---|
| 0 | 成功 |
| 1 | 意外的内部错误 |
| 2 | 用法或命令语法错误 |
| 3 | 策略拒绝，或请求的前置条件不成立 |
| 4 | 操作失败，未留下持久改动 |
| 5 | 部分完成、已应用但仍有后续步骤待收尾，或内核视图过期（补救援引会打印） |
| 6 | 平台或能力不支持 |

`--sector-size N` 只对镜像文件适用：镜像不携带扇区信息，默认按 512 字节扇区读，4Kn 镜像必须显式指定。块设备的逻辑扇区大小由内核报告，对它给出该选项会被拒绝。

## 文档

- [docs/zh/reference.md](docs/zh/reference.md) — 完整命令参考：每条命令的语法、选项、详助与退出状态。由命令 schema 生成，不要手改；英文版见 [docs/en/reference.md](docs/en/reference.md)。
- [docs/zh/capability-matrix.md](docs/zh/capability-matrix.md) — 每条命令与每项能力在“平台 × 介质”上的矩阵。生成物，不要手改；英文版见 [docs/en/capability-matrix.md](docs/en/capability-matrix.md)。
- [docs/zh/internals.md](docs/zh/internals.md) — 行为边界：能力门、占用与并发、目标身份、恢复记录、续跑语义、外部工具、验证范围。
- [docs/zh/interop.md](docs/zh/interop.md) — 在工具之外处理 qcow2/vmdk/vdi、远程目标与稀疏镜像：命令、接缝，以及哪些未经实测。

## License

MIT，见 [LICENSES/](LICENSES/)。