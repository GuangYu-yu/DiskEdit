# DiskEdit

把磁盘与磁盘镜像统一当作**可按偏移读写的字节存储**，在其上做分区表编辑、文件系统操作与分区搬移。

- **分区表**：GPT/MBR 的解析与写入不经过 libparted/sfdisk。GPT 提交按「备条目数组 → 备头 → 主条目数组 → 主头」四步，每步 `sync`，任意落点断电都至少留一份自洽副本，不一致可经 CRC 检出。块设备上挂载中的分区内核重读必然失败，该路径改用 `sfdisk --no-reread` 写表 + `partx -u` 同步内核。
- **文件系统**：mkfs/fsck/resize/label/uuid 经 offset loop（`losetup`）调用各 FS 官方工具。
- **镜像与块设备**：`.img` 与 `/dev/sdX` 走同一条代码路径，差别只在打开方式。

## 运行要求

**权限**：

| 操作 | 镜像文件 | 块设备 |
|---|---|---|
| `info` / `plan` | 不需要 root | 不需要 root |
| 分区表写入（`new`/`add`/`del`/`resize-part`/`copy`/`set`/`undo`/`move`/`apply`） | 不需要 root | 需要 root |
| FS 层（`mkfs`/`resizefs`/`check`/`set label\|uuid`） | 需要 root | 需要 root |

`resize` 分两种：需要动文件系统时要 root；加 `--no-fs` 表示不把 FS 增长算作后置条件（镜像上无需 root）。目标 FS 无法识别（`unknown`）时，不带 `--no-fs` 一律拒绝——"认不出"不等于"里面没有东西"。

FS 层要 root 是因为经 `losetup` 映射分区；块设备写入靠设备节点权限，权限不足时在打开阶段即失败。

**占用边界**：块设备写入靠整盘 `O_EXCL` 独占打开——任一分区被挂载或作 swap 时，内核以 EBUSY 拒绝打开；写盘前另有占用闸复核（块设备上逐分区探测，是纵深防御）。镜像文件没有这层内核语义，占用闸改为反向枚举 loop：以已打开镜像句柄的 `(st_dev, st_ino)` 与 `LOOP_GET_STATUS64` 返回的 `(lo_device, lo_inode)` 比对，命中后检查该 loop 及其分区是否被挂载（遍历所有可读 mount namespace）、是否为活动 swap、是否被块设备栈持有。判定不依赖 `loop/backing_file` 路径，硬链接别名、已删除的 backing 路径、其它 mount namespace 内的挂载都能命中。**直接改表的命令**（`new`/`add`/`del`/`create`/`set name|type|flag`/`undo`）要求目标完全空闲：任一分区在用都会让新表与内核视图分叉，故不区分本次改哪个分区。`copy` 是例外——它只在经重叠校验的空闲区新增条目，不改既有分区的起止，也不搬移目标分区的 extent。搬移类命令（`resize-part`/`move`/`resize`/`apply`）与 `mkfs` 按操作形状区分：起点不变、末端只增的写入放行"仅被 dm 持有"的目标（LVM PV / dm-crypt 的增长语义），搬移、缩小与整段重写一律拒绝。探测中任一枚举到的 loop 节点打不开、ioctl 返回 ENXIO 之外的错误、sysfs 读失败、`/proc/<pid>/mountinfo` 因权限读不到（如 `hidepid`），都按"无法确认"拒绝写入（退出码 30）——即使那个 loop 与目标无关；这是安全优先的运行环境限制，不能当作目标被占用的证据。

**LVM 收尾的边界**：镜像内 PV 的 `pvresize`/`lvextend` 经临时 loop 映射执行。若该 PV 已被另一个 loop 映射且 VG 中有活动 LV，LVM 会在同一 PV 的两个设备视图间优先选择"已被 LV 使用"的那个，对临时映射的 `pvresize` 不生效（退出码仍为 0）。本工具据后置条件判据报 PARTIAL(20) 并打印补救指引，不宣称成功；分区表扩展已提交，PV、LV 与 FS 均未被改动。要在这种配置下完成 LVM 收尾，需先停用 VG、释放用户侧的 loop 再重跑。

镜像 PV 上 `--grow-lv` 成功时，`lvextend -r` 会把原本非活动的 LV 留在活动状态，临时 loop 因此不能立即解除：`losetup -d` 返回成功，内核把该 loop 标为 autoclear，映射在 VG 停用（`vgchange -an`）时随之释放。这是临时映射的生命周期，不是泄漏；停用 VG 后 `losetup -a` 不再列出该镜像。

**并发边界**：目标的独占锁是 *advisory* 的——它只在**本工具**的各次调用之间互斥，不阻止其它程序直接写同一个目标（机制见 `src/targetlock.rs` 模块注释）；块设备的整盘 `O_EXCL` 与挂载占用闸只挡内核语义内的写者。镜像的 loop 反查是写盘前的时点判定：检查完成到实际写入之间，外部进程仍可能新建 loop、启动 swap 或挂载该镜像，本工具的锁不约束这些进程。分区表写入在解析期读一次表，提交期写回的是那份快照；提交前会重读条目数组并与快照逐条比对（本层改过的槽位允许等于目标值），任何一条对不上即拒绝写表，外部进程在窗口内的改动因此不会被静默覆盖（修复步走同一判据，覆盖 add/rename/set/del 与搬移、resize 的全过程）。要与其它写者共用同一目标，请在外部自行串行化。非 Unix 平台上镜像身份按字面路径派生、不做 realpath：同一文件的硬链接 / 符号链接 / junction 别名会各拿一把锁、各有一份现场，经别名路径的并发使用在支持边界之外。

**设备身份边界**：块设备的 journal / checkpoint / 锁按身份键落在系统状态目录下，身份键优先取设备标识属性（wwid / dm-uuid / md-uuid / serial），都取不到时退化为 `{sysfs 节点名}-{容量扇区数}`（见 `src/dev.rs`）。退化键里没有绑定具体物理盘的成分：同槽位换入同节点名、同容量的另一块盘会落进同一身份域，上一块盘留下的现场会被新盘继承（未收尾的作业表现为目标被占用；`undo` 的分叉核对以盘上字节判定，逐字节相同的克隆盘不构成区分）。镜像目标按路径派生身份，标识的是"这个路径"而非"这个文件"。

**恢复现场的身份核对**：journal 与 checkpoint 随现场记下创建时的目标指纹，回放 / 续跑前逐字段比对，任一**可比字段**不符即拒绝。字段的证据强度按平台与载体分级：尺寸（`Metadata::len()`）处处可比；device id / inode 在 Unix 上取 `Metadata` 的扩展字段，在 Windows 上从句柄取 (卷序列号, 文件索引)，两者之外的平台恒零，真块设备的节点上同样恒零（`udev` 重建节点即变，节点元数据长度也恒 0）。零值表示**给不出这个字段**，不参与比对，也不被当作匹配——那类载体上的核对退化为"落点由身份键派生 + 盘上记录逐条核对"，`undo` 会打印一行 `note` 说明这一点。尺寸本身不足以唯一标识目标：同尺寸的替换文件仍可能通过该字段。

**验证范围**：本工具按 Linux 的 `util-linux` / 各 FS 官方工具的命令行契约工作，不绑定发行版；回归套件（`tests/wsl/`）在 Ubuntu + `util-linux` 2.41.3 上验证，其他发行版与版本未经验证。占用探测按下列分档记录，不把未跑过的路径说成已通过：

| 证据等级 | 内容 |
|---|---|
| 已运行验证 | loop 身份编码（`st_dev`/`st_ino` 对 `lo_device`/`lo_inode`）、未绑定 loop 的 `ENXIO`、硬链接别名与已删除 backing 路径、跨 mount namespace 的挂载命中、loop 与分区挂载、活动 swap、sysfs holders、已退出进程的 mountinfo（`EINVAL`）、探测失败 fail-closed（退出码 30）、LVM PV 无持有者时的 `--grow-lv` 全链、活动 LV 持有分区时 `AllowDm` 不被通用规则拒绝（链进入 LVM 步）且 `mkfs` 整段重写仍被拒、挂载镜像上 `new`/`add`/`del`/`create`/`set name\|type\|flag`/`undo` 全部被拒且盘面字节不变、多个 loop 命中时拒绝消息指向实际持有者、非 root 用户（打不开 `root:disk` 的 loop 节点）的镜像操作可用 |
| 未验证 | 容器与独立 PID namespace 组合下的 loop 枚举完整性；`hidepid` 等权限限制下 mountinfo 不可读的实际部署（代码按探测失败拒绝，未在真实 `hidepid` 环境运行验证）；上表之外的发行版与内核版本；**镜像内 PV 已被另一 loop 映射且 VG 有活动 LV 时，`--grow-lv` 的完整收尾链**（见上节，LVM 重复 PV 规则使 `pvresize` 对临时映射 no-op，本工具报 PARTIAL(20)，该场景下工具不宣称成功） |
| 设计限制 | 占用闸是写盘前的时点判定，不阻止检查后外部新建 loop / swap / 挂载；镜像侧枚举只覆盖本进程可见的 loop 节点，因权限不足打不开的 loop 节点（`root:disk` 权限）按未绑定跳过——绑定与节点访问同权限类，该类用户建不出指向本镜像的绑定；节点不存在、`ENODEV` 等无法确认身份的情形按探测失败拒绝，此边界随内核/udev 权限模型演进；fail-closed 对无关 loop 的探测失败同样生效；journal / checkpoint 的零值字段不参与比对 |

**外部工具**（按需存在即可，用到才查找）：

| 用途 | 工具 |
|---|---|
| 内核同步 / 在线写表 | `sfdisk`、`partx` |
| loop 与挂载 | `losetup`、`udevadm`、`mount`、`umount` |
| ext2/3/4 | `mke2fs`、`e2fsck`、`resize2fs`、`dumpe2fs`、`tune2fs` |
| xfs | `mkfs.xfs`、`xfs_growfs`、`xfs_repair`、`xfs_admin` |
| btrfs | `mkfs.btrfs`、`btrfs`、`btrfstune` |
| ntfs | `mkfs.ntfs`、`ntfsresize`、`ntfsfix`、`ntfslabel` |
| f2fs | `mkfs.f2fs`、`fsck.f2fs`、`resize.f2fs` |
| vfat | `mkfs.vfat`、`fsck.vfat`、`fatresize`、`fatlabel` |
| exfat | `mkfs.exfat`、`fsck.exfat`、`exfatlabel` |
| swap | `mkswap`、`swaplabel` |
| LVM | `pvs`、`lvs`、`vgs`、`pvresize`、`lvextend` |

所有外部调用注入 `LC_ALL=C`，避免本地化输出破坏解析。

## 命令

`<TARGET>` 为镜像路径或块设备；`:N` 寻址第 N 个分区（1 起），`:last` 指最后一个可操作分区（MBR 上扩展容器不算，无分区表的目标两者都不接受），`--grow` 同此两形。`diskedit help <CMD>` 查看单命令详助。

| 命令 | 作用 | 写盘 |
|---|---|---|
| `info <TARGET>` | 分区表、逐分区 FS 识别（LVM PV 显示为 lvm2_pv） | 否 |
| `resize <TARGET>:N <SIZE>` | 分区 + FS 一起扩缩，自动选在线/离线 | 是 |
| `move <TARGET>:N --start <LBA\|end>` | 移动分区，数据跟随 | 是 |
| `copy <TARGET>:N --start <LBA\|end> [--name S]` | 复制分区到新位置，源不动 | 是 |
| `create <TARGET> [--size B] [--name S] [--fs F]` | 在空闲空间创建分区 | 是 |
| `delete <TARGET>:N --yes` | 删除分区表项（不擦数据区） | 是 |
| `set <TARGET>:N name S \| label S \| uuid U \| flag F on\|off` | 设置分区属性 | 是 |
| `check <TARGET>:N` | FS 一致性检查 | 视工具 |
| `mkfs <TARGET>:N <FS> --yes` | 创建文件系统 | 是 |
| `resizefs <TARGET>:N` / `<MOUNTPOINT> [BYTES] --online` | 单独调整 FS | 是 |
| `undo <TARGET> --yes` | 按 journal 撤销本工具的写入 | 是 |
| `new` / `add` / `del` / `resize-part` / `plan` / `apply` | 低阶命令 | 见详助 |

`resize` 的 `SIZE` 支持绝对值（`10G`）、增量（`+2G` / `-500M`）、百分比（`+10%` / `-10%`）与 `grow` 关键字。

`--sector-size N` 是全局选项：镜像文件不携带扇区信息，默认按 512 处理，4Kn 镜像须显式指定。`resizefs` 的在线形式（挂载点）不适用该选项，显式拒绝。

**`check` 不是零写入**：ext 走 `e2fsck -fp`（preen 自动修复），ntfs 走 `ntfsfix -d`（清 dirty 位），两者都可能写入。只读检查为 xfs（`xfs_repair -n`）、btrfs（`btrfs check`）、vfat（`fsck.vfat -n`）、exfat（`fsck.exfat -n`）、f2fs（`fsck.f2fs` 无参）。

**LVM PV 不能用 `mkfs` 重建**：工具显式拒绝，须用 `pvcreate(8)` / `wipefs(8)`。

## 支持的 FS

**识别**（`fsid.rs`，按魔数）：ext、xfs、btrfs、f2fs、ntfs、vfat、exfat、swap、squashfs、erofs、hfsplus、apfs、lvm2_pv；都不匹配则为 `unknown`。

**动作覆盖**：

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
| lvm2_pv | — | ✓ | — | — | — | — |

- `ntfs` 的 uuid 只能生成随机新序号，`ntfslabel` 不支持指定值。
- `swap` 的扩容是扩完表项后用 `mkswap` 重建，不搬数据；UUID、PARTUUID 与分区号保持。
- `lvm2_pv` 的扩容只到 PV 层，加 `--grow-lv` 才继续扩 LV 及其文件系统；缩容一律拒绝——链与扩容反向（先 `lvreduce -r` 缩 LV+FS，再 `pvresize --setphysicalvolumesize`，最后改分区表），本工具只管后者。

缩容时先缩 FS、后改分区边界；FS 缩不动就不动表。

**OpenWrt combined 布局**（同分区内 squashfs/erofs 只读根 + 尾部 RW overlay）：扩容识别内层 FS 后经 offset loop 只扩 RW 层，只扩不缩；内层非 ext/f2fs 时拒绝。尾部 RW 层尚未格式化（首启安装现场）不算失败：分区照扩，该层由首次挂载时的 fstools 初始化，本次无事可做。多设备 btrfs 拒绝扩容。

## License

MIT（见 `LICENSES/`）