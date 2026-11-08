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
| 分区表写入（`new`/`add`/`delete`/`resize-part`/`copy`/`set`/`undo`/`move`） | 不需要 root | 需要 root |
| FS 层（`mkfs`/`resizefs`/`check`/`set label\|uuid`） | 需要 root | 需要 root |

`resize` 分两种：需要动文件系统时要 root；加 `--no-fs` 表示不把 FS 增长算作后置条件（镜像上无需 root）。目标 FS 无法识别（`unknown`）时，不带 `--no-fs` 一律拒绝——"认不出"不等于"里面没有东西"。

FS 层要 root 是因为经 `losetup` 映射分区；块设备写入靠设备节点权限，权限不足时在打开阶段即失败。

**平台能力门**：只有 Linux 承诺安全写盘；其他平台在命令分派处一律按能力缺口拒绝——没有任何命令可用，`info`/`plan`/`inspect`/`records`/`check`/`abandon` 与所有写命令都不例外，退出码 6，文案为 `no command is available on this platform — diskedit supports Linux only. Nothing was written (no target bytes, lock, recovery record, or log).`。能力门在几何计算、取锁与任何伴随文件创建之前，先于确认门。

**占用边界**：块设备写入靠整盘 `O_EXCL` 独占打开——任一分区被挂载或作 swap 时，内核以 EBUSY 拒绝打开；写盘前另有占用闸复核（块设备上逐分区探测，是纵深防御）。镜像文件没有这层内核语义，占用闸改为反向枚举 loop：以已打开镜像句柄的 `(st_dev, st_ino)` 与 `LOOP_GET_STATUS64` 返回的 `(lo_device, lo_inode)` 比对，命中后检查该 loop 及其分区是否被挂载（遍历所有可读 mount namespace）、是否为活动 swap、是否被块设备栈持有。判定不依赖 `loop/backing_file` 路径，硬链接别名、已删除的 backing 路径、其它 mount namespace 内的挂载都能命中。本进程登记的 loop 独占权是进程级名字表，条目每次消费都按当前活映射复核（名字只用于找回登记）；占用判定以 backing 的 `(st_dev, st_ino)` 命中该文件的全部 loop，因此能判“这个文件正被映射/持有”，不区分同一文件系统上重复叠挂的是哪一条。**直接改表的命令**（`new`/`add`/`delete`/`set name|type|flag`/`undo`）要求目标完全空闲：任一分区在用都会让新表与内核视图分叉，故不区分本次改哪个分区。`copy` 是例外——它只在经重叠校验的空闲区新增条目，不改既有分区的起止，也不搬移目标分区的 extent。搬移类命令（`resize-part`/`move`/`resize`）与 `mkfs` 按操作形状区分：起点不变、末端只增的写入放行"仅被 dm 持有"的目标（LVM PV / dm-crypt 的增长语义），搬移、缩小与整段重写一律拒绝。探测中任一枚举到的 loop 节点打不开、ioctl 返回 ENXIO 之外的错误、sysfs 读失败、`/proc/<pid>/mountinfo` 因权限读不到（如 `hidepid`），都按"无法确认"拒绝写入（退出码 4）——即使那个 loop 与目标无关；这是安全优先的运行环境限制，不能当作目标被占用的证据。

**LVM 收尾的边界**：镜像内 PV 的 `pvresize`/`lvextend` 经临时 loop 映射执行。若该 PV 已被另一个 loop 映射且 VG 中有活动 LV，LVM 会在同一 PV 的两个设备视图间优先选择"已被 LV 使用"的那个，对临时映射的 `pvresize` 不生效（退出码仍为 0）。本工具据后置条件判据报退出码 5（后置视图过期）并打印补救指引，不宣称成功；分区表扩展已提交，PV、LV 与 FS 均未被改动。要在这种配置下完成 LVM 收尾，需先停用 VG、释放用户侧的 loop 再重跑。本工具不替其它持有栈抬高 loop 的 `sizelimit`：外部 loop 与本次请求 offset 相同、但 `sizelimit` 不一致（抬高与收缩都算）时直接拒绝，文案指向由持有该映射的那一层解绑并重绑（`losetup -d <dev>`，让栈重新映射）后再试；`--grow-lv` 同理，持有方不先重绑就不会替你完成 LVM 收尾。

镜像 PV 上 `--grow-lv` 成功时，`lvextend -r` 会把原本非活动的 LV 留在活动状态，临时 loop 因此不能立即解除：`losetup -d` 返回成功，内核把该 loop 标为 autoclear，映射在 VG 停用（`vgchange -an`）时随之释放。这是临时映射的生命周期，不是泄漏；停用 VG 后 `losetup -a` 不再列出该镜像。

**并发边界**：目标的独占锁是 *advisory* 的——**它只约束遵守 DISKEDIT 协议的进程**，在这些进程的各次调用之间互斥；**这不是对所有系统写入者的强制锁**，不阻止其它程序、内核侧写入或不经过本工具的路径直接写同一个目标，不能读成系统级独占保证（机制见 `src/targetlock.rs` 模块注释）；块设备的整盘 `O_EXCL` 与挂载占用闸只挡内核语义内的写者。镜像的 loop 反查是写盘前的时点判定：检查完成到实际写入之间，外部进程仍可能新建 loop、启动 swap 或挂载该镜像，本工具的锁不约束这些进程。分区表写入在解析期读一次表，提交期写回的是那份快照；提交前会重读条目数组并与快照逐条比对（本层改过的槽位允许等于目标值），任何一条对不上即拒绝写表，外部进程在窗口内的改动因此不会被静默覆盖（修复步走同一判据，覆盖 add/rename/set/delete 与搬移、resize 的全过程）。要与其它写者共用同一目标，请在外部自行串行化。inode 级第二道凭据只在 Unix 上取，**非 Unix 平台只承诺锁文件这一道**，硬链接 / 符号链接 / junction 别名各拿一把锁、各有一份现场，经别名路径的并发使用在支持边界之外（`std::fs::File::try_lock` 在 Windows 上是强制锁且锁整个文件，不能替代这道凭据）。

**设备身份边界**：块设备的恢复记录按目标的 **identity-hash** 落在 `<state_root>/recovery/<identity-hash>/record`——目录名取恒等字段（设备类别、稳定身份串、载体 `file_id`）按序拼接后 sha256 的前 16 字节小写 hex；分区目标的稳定身份串是载体稳定串加分区号，故分区节点与整盘节点各有一份记录目录；锁不按目标派生，按整盘盘键落在 `<state_root>/<盘键>.lock`，两者共用一把。状态根默认 `/var/lib/diskedit`；Linux 上 euid 非 0 时忽略 `DISKEDIT_STATE_DIR`，其余平台没有块设备目标，沿用镜像侧的状态目录。稳定身份按 `wwid` → `device/wwid` → 两次读数一致的 `device/serial` 取第一个非空原文；dm、md、只有 major:minor、读不出或两次读数不一致的目标是 `Unusable`——没有记录目录，写命令以 6 拒绝。sysfs 拓扑（节点、`partition` 属性、父设备）读不出来时同样**拒绝**，不退化成调用方路径的 `{devname}-{容量}`。容量不参与分区身份（见 `src/dev/identity.rs`、`src/dev/topology.rs`、`src/dev/host_repo.rs`）。镜像身份按路径派生，标识的是"这个路径"而非"这个文件"。Linux 上 realpath 只合并符号链接与路径写法，**不合并硬链接**：同一 inode 的两个硬链接名各派生一份记录目录（`<名>.diskedit/`），经 A 名中断的作业在 B 名上不可见，也无法从 B 名续跑或 undo——这类作业要用**同一个名字**收尾。锁有两层：**锁文件**（块设备按整盘盘键落在 `<state_root>/<盘键>.lock`，整盘与它的分区共用一把；镜像落在 `<image>.diskedit/record.lock`；loop 落在 backing 旁的兄弟锁文件；盘键有 device-id 时取它，否则取 sysfs 父节点名 + 容量扇区）与 **inode 级第二道凭据**（仅 Unix，对 `data_path` 重新打开的句柄本身加锁：镜像落在数据文件上，块设备落在设备节点上）。后者把同一 inode 的别名路径收敛到一把锁；整盘与分区由第一道的共用锁文件收敛。**非 Unix 平台只承诺锁文件这一道**，硬链接 / 符号链接 / junction 别名各拿一把锁、各有一份现场，经别名路径的并发使用在支持边界之外（`std::fs::File::try_lock` 在 Windows 上是强制锁且锁整个文件，不能替代这道凭据）。

**恢复现场的身份核对**：恢复记录随记录记下创建时的目标（`SceneIdentity`）：身份材料（块设备取设备 ID，镜像取文件 ID）、证据种类、载体容量、loop 映射（offset 与 sizelimit）与载体文件身份；回放 / 续跑前逐字段比对，任一**可比字段**不符即拒绝。证据种类是恒等字段：记录写下的种类与当前目标给出的种类不同，即判不符。文件 ID 在 Unix 上取 `(st_dev, st_ino)`，在 Windows 上取卷序列号 + 文件索引；卷序列号本身是画像字段，不参与身份判定。容量与载体文件 ID 只在两侧都拿得出时比较，缺项不参与；loop 映射必须一致，一侧有映射、另一侧没有按不符处理。容量本身不足以唯一标识目标：同尺寸的替换文件仍可能通过。一个稳定身份都拿不到的目标（`Unusable`）在**任何字节落盘之前**就被拒绝：那种现场中断后既无法续跑也无法 undo，不能等恢复时才发现无路可走；记录侧留下「无证据」的现场永远不会与任何当前目标匹配，回放前的核对以 3 拒绝并给出原因。

**恢复记录的盘 GUID 凭据**：记录（格式 v6，magic `DSKR`）在身份段之外再记目标 GPT 的 Disk GUID，三态为 `Present` / `Unavailable` / `GeneratedThisOperation`。`Present` 必须与盘面一致；`Unavailable` 表示目标上没有可读的 GPT（MBR / 裸盘），不构成证据；`GeneratedThisOperation` 只出现在记录侧（整张表由写下这条记录的同一操作生成），与盘面 GUID 不做比对。两侧都 `Present` 而值不同、记录 `Present` 而目标已无可读 GPT、记录 `Unavailable` 而目标现在带 GPT，都按身份不符拒绝。**旧格式不迁移**：不是本构建认识的记录格式版本按 `UnrecognizedFormat` 处理，写命令以 3 拒绝；本构建既不回放也不迁移。`abandon` 的自动隔离以头、版本、身份段 CRC、身份链全部通过为前提。`undo` 对旧 undo journal 以 3 拒绝并说明它早于记录格式，不迁移、不回放、不删除；无法确认它是否存在时以 4 拒绝。

**恢复记录的落点**：恢复记录是落点上的一份普通文件（镜像 `<image>.diskedit/record`，块设备 `<state_root>/recovery/<identity-hash>/record`）。落点上是什么由一次探测回答：缺席、可解码、损坏（撕裂或 CRC 不符）、不认识的格式版本、不是恢复文件（符号链接 / 目录 / 非普通文件）、读不出。Linux 上从父目录锚点逐级 `openat(O_NOFOLLOW|O_DIRECTORY)` 解析，末级 `O_NOFOLLOW`：任一级符号链接、非目录，或末级不是普通文件，都按“不是恢复文件”处理。非 Linux 是预留端口，只按 `symlink_metadata` 回答存在与否、内容一律不读：普通文件在这里按“读不出”拒绝，本构建在该平台上只能拒、不能读记录。删除与改名都对该路径入口本身（父目录 fd + 相对名）执行，不跟随链接、不改动链接目标；原子替换走同目录改名。条目不是恢复文件或删除失败时按“现场仍在”处理，不报干净。

**Windows 的目录项持久化为预留**：删除或改名恢复文件后，本工具不宣称目录项已落盘，只打印 `note: removed <path>; persistence of the directory entry was not verified on this platform`（改名同理）。该平台不提供目录项掉电持久性保证，不能把这句 note 当作“已持久化”。

**验证范围**：本工具按 Linux 的 `util-linux` / 各 FS 官方工具的命令行契约工作，不绑定发行版；回归套件（`tests/wsl/`）在 Ubuntu + `util-linux` 2.41.3 上验证，其他发行版与版本未经验证。占用探测按下列分档记录，不把未跑过的路径说成已通过：

| 证据等级 | 内容 |
|---|---|
| 已运行验证 | loop 身份编码（`st_dev`/`st_ino` 对 `lo_device`/`lo_inode`）、未绑定 loop 的 `ENXIO`、硬链接别名与已删除 backing 路径、跨 mount namespace 的挂载命中、loop 与分区挂载、活动 swap、sysfs holders、已退出进程的 mountinfo（`EINVAL`）、探测失败 fail-closed（退出码 4）、LVM PV 无持有者时的 `--grow-lv` 全链、活动 LV 持有分区时 `AllowDm` 不被通用规则拒绝（链进入 LVM 步）且 `mkfs` 整段重写仍被拒、挂载镜像上 `new`/`add`/`delete`/`set name\|type\|flag`/`undo` 全部被拒且盘面字节不变、多个 loop 命中时拒绝消息指向实际持有者、非 root 用户（打不开 `root:disk` 的 loop 节点）的镜像操作可用 |
| 未验证 | 容器与独立 PID namespace 组合下的 loop 枚举完整性；`hidepid` 等权限限制下 mountinfo 不可读的实际部署（代码按探测失败拒绝，未在真实 `hidepid` 环境运行验证）；上表之外的发行版与内核版本；**镜像内 PV 已被另一 loop 映射且 VG 有活动 LV 时，`--grow-lv` 的完整收尾链**（见上节，LVM 重复 PV 规则使 `pvresize` 对临时映射 no-op，本工具报退出码 5，该场景下工具不宣称成功） |
| 设计限制 | 占用闸是写盘前的时点判定，不阻止检查后外部新建 loop / swap / 挂载；镜像侧枚举只覆盖本进程可见的 loop 节点，因权限不足打不开的 loop 节点（`root:disk` 权限）按未绑定跳过——绑定与节点访问同权限类，该类用户建不出指向本镜像的绑定；节点不存在、`ENODEV` 等无法确认身份的情形按探测失败拒绝，此边界随内核/udev 权限模型演进；fail-closed 对无关 loop 的探测失败同样生效；记录的缺项（`None`）不参与比对 |

**续跑边界**：中断后重跑原命令即按恢复记录续跑的命令是 `new`、`move`、`resize-part`、`mkfs`、`resizefs`、`set` 与 GPT 的 `resize`；`add`、`delete`、`copy` 没有续跑路径（`help copy` 明确 `No resume`），重跑前先用 `undo` 回滚或用 `abandon` 释放现场。MBR 的 `resize` 中断时打印 `note: MBR has no recovery record — if this grow is interrupted, inspect the partition table and filesystem size before re-running`，不把重跑当续跑。缩容步进记三态（未缩 / 已验证 / 未验证）：回读不出 FS 尺寸时停在缩容这一步并按退出码 5 报未收尾，重跑先回读实际尺寸，确认未缩才重做一次缩容。收尾时恢复记录只在六条同时成立时才删除：记录可识别、提交状态为已提交、身份相符、持久化已确认、后置回读通过、表写步骤已完成；不成立即保留，并打印 `the recovery record was not cleaned up (<原因>) — release it with diskedit abandon`。删除本身失败时事务报退出码 5 而不是成功。搬移的数据按批同步并推进 durable 边界（同步先于记录推进，边界永不超前于已落盘字节）；提交批量按提交耗时自适应，崩溃后最多重做 256 MiB 已拷数据，块字节大于该窗口时退化为逐块提交。自重叠搬移（位移小于源区间长度）无论哪条路径都逐块提交——批量攒下的未持久化块会探入重做区间，重做会读到被同批写覆盖的源数据。

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

`<TARGET>` 为镜像路径或块设备；`:N` 寻址第 N 个分区（1 起），`:last` 指最后一个可操作分区（MBR 上扩展容器不算，无分区表的目标两者都不接受）。`diskedit help <CMD>` 查看单命令详助。

| 命令 | 作用 | 写盘 |
|---|---|---|
| `info <TARGET>` | 分区表、逐分区 FS 识别（LVM PV 显示为 lvm2_pv） | 否 |
| `inspect <TARGET>` / `inspect <PATH>` | 只读查看目标旁的恢复记录（无记录/可清理/未收尾/损坏），不接受 `:N` 与 `--sector-size` | 否 |
| `resize <TARGET>[:N] <SIZE>` | 分区 + FS 一起扩缩，自动选在线/离线 | 是 |
| `move <TARGET>:N --start <LBA\|end> --chunk N --yes` | 移动分区，数据跟随 | 是 |
| `copy <TARGET>:N --start <LBA\|end> --chunk N [--name S] [--keep-partuuid] --yes` | 复制分区到新位置，源不动 | 是 |
| `delete <TARGET>:N --yes` | 删除分区表项（不擦数据区） | 是 |
| `set <TARGET>:N name S \| label S \| uuid U \| flag F on\|off \| type GUID \| type 0xXX --yes` | 设置分区属性 | 是 |
| `check <TARGET>:N` | FS 一致性检查（只读） | 否 |
| `mkfs <TARGET>:N <FS> --yes [--label S]` | 创建文件系统 | 是 |
| `resizefs <TARGET>:N --yes` / `resizefs <MOUNTPOINT> --yes [BYTES]` | 单独调整 FS（形态即模式：`:N` 离线，挂载点在线） | 是 |
| `undo <TARGET> --yes` | 按恢复记录撤销本工具的写入；旧 undo journal 不是恢复记录：不迁移、不回放、不删除，存在时以 3 拒绝（`help undo`） | 是 |
| `abandon <TARGET> [--discard-replay]` | 放弃未收尾的现场：把记录移入隔离区（`.abandoned[.N]`），不动目标字节；旧 journal / 旧 checkpoint 不是恢复记录，不认领、留在原处（`help abandon`） | 否 |
| `records list [TARGET]` | 以 JSON 列出可解码的恢复记录（无索引，逐条标 `registered: false`）；未知格式不进列表 | 否 |
| `records verify <ID> <TARGET>` | 按记录编号定位唯一一条并重新核对目标身份，输出 JSON（0 通过 / 3 未找到、命中多处或核对失败 / 4 读不出） | 否 |
| `records scan <ROOT>` | 枚举容器根下的记录并输出 JSON；有界扫描（深度 3、单文件 64 MiB、总量 512 MiB、30 s），越界报结果不完整并退 4 | 否 |
| `plan <TARGET>[:N]` | 只读报告：几何、表、FS 与用量结论（字段同 `info`）；`--grow` 追加 `:N` 的搬迁预览 | 否 |
| `new` / `add` / `resize-part` | 低阶命令 | 见详助 |

`resize` 的 `SIZE` 支持绝对值（`10G`）、增量（`+2G`）、百分比（`+10%`）与 `grow` 关键字；它只扩不缩，收缩请求拒绝（3），缩容走 `resizefs` + `resize-part` 两步。

`--sector-size N` 只对镜像文件适用：镜像不携带扇区信息，默认按 512 处理，4Kn 镜像须显式指定。块设备的逻辑扇区大小由内核报告（BLKSSZGET），显式给出即拒绝；各命令是否接受该选项以 `help <CMD>` 为准（`inspect`、`abandon`、`records` 不接受，`resizefs` 的在线形式（挂载点）显式拒绝）。

## 选项

各命令实际接受哪些选项以 `help <CMD>` 为准；下表是全部选项与取值形态，取值列为 `—` 的选项不带值。

| 选项 | 取值 | 含义 |
|---|---|---|
| `--align` | A | 放置对齐：`mib` 或 `none`（默认 `none`，按给定 LBA 使用；`add`、`move`、`copy`、`resize-part` 接受） |
| `--allow-move` | — | `resize` 在 GPT 上允许搬开挡路的分区 |
| `--chunk` | MiB | `move`、`copy`、`resize` 的恢复粒度（MiB，1..=64）；`move`/`copy` 必填，`resize` 仅在 `--allow-move` 时必填 |
| `--discard-replay` | — | `abandon` 在记录仍有可回放区间（或载荷损坏、条数未知）时必须显式给出；所有命令都接受该旗标 |
| `--end` | LBA | `resize-part` 的结束 LBA |
| `--grow-lv` | — | `resize` 在 LVM PV 上继续扩 LV 及其文件系统 |
| `--grow` | — | `plan` 在只读报告后追加 `:N` 的搬迁预览（不落盘、不产出可复用计划） |
| `--grow-to-end` | — | `resize-part` 末端钉在可用空间末尾 |
| `--keep-partuuid` | — | `copy` 沿用源分区的 PARTUUID |
| `--label` | S | `mkfs` 创建时写入的文件系统标签（省略则写空标签，不造默认值） |
| `--lv` | NAME | `--grow-lv` 要扩的 LV（省略时取该 PV 上唯一的 LV） |
| `--name` | S | 分区名 |
| `--no-fs` | — | 跳过文件系统步骤 |
| `--overwrite-data` | — | `new` 授权覆盖旧分区落入新表元数据写入区的字节 |
| `--part-number` | N | `add` 指定条目槽位（1 起，须为空；省略取第一个未用槽位） |
| `--random` | — | `set uuid` 要求目标自行生成新值：ext 用 `tune2fs -U random`、xfs 用 `xfs_admin -U generate`、btrfs 用 `btrfstune -u`；swap 没有生成形式，ntfs 的序号不是 Windows 卷 UUID |
| `--sector-size` | N | 镜像逻辑扇区大小（2 的幂，不低于 512 字节；上限 65536 字节是实现限制，不是格式要求）；`copy` 只对一个目标生效 |
| `--start` | LBA\|end | 起点 LBA；`add` 省略时取首个可容纳 SIZE 的空洞；`move`、`copy` 必填并另接受 `end`；`resize-part` 只能等于当前起点，搬移请用 `move` |
| `--table` | gpt\|mbr | `new` 创建的分区表类型 |
| `--type-guid` | GUID\|0xNN | `add` 的分区类型：GPT 用标准 GUID 文本（省略取 Linux filesystem data）；MBR 用两位十六进制类型字节（省略取 0x83） |
| `--yes` | — | 写入前的确认门：`new`、`add`、`delete`、`move`、`copy`、`resize-part`、`resize`、`mkfs`、`resizefs`、`set`、`undo` 一律必填，缺失在解析期退出 2；其余命令（`info`、`inspect`、`records list/verify/scan`、`check`、`abandon`、`plan`）不接受该旗标 |

**`check` 是只读零写**：各工具一律以只读形式运行——ext `e2fsck -n`、ntfs `ntfsfix -n`、xfs `xfs_repair -n`、btrfs `btrfs check`、vfat `fsck.vfat -n`、exfat `fsck.exfat -n`、f2fs `fsck.f2fs`（无参）。工具退出非零即检查失败（退出码 3）。

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

- `ntfs` 的 uuid 只能生成新序号（`ntfslabel --new-serial`），不支持指定值；该序号不是 Windows 卷 UUID。
- `vfat` 的 label 在本工具内只收 ASCII：卷标字段是 11 **字节**、按 DOS/OEM 代码页存放，本工具不做代码页转换（非 ASCII 标签请直接用 `fatlabel` 设置）。
- `swap` 的扩容是扩完表项后用 `mkswap` 重建，不搬数据；UUID、PARTUUID 与分区号保持。
- `lvm2_pv` 的扩容只到 PV 层，加 `--grow-lv` 才继续扩 LV 及其文件系统；缩容一律拒绝——链与扩容反向（先 `lvreduce -r` 缩 LV+FS，再 `pvresize --setphysicalvolumesize`，最后改分区表），本工具只管后者。

缩容不经过 `resize`：先 `resizefs` 缩文件系统，再 `resize-part` 缩分区边界；FS 缩不动就不动表。

**OpenWrt combined 布局**（同分区内 squashfs/erofs 只读根 + 尾部 RW overlay）：扩容识别内层 FS 后经 offset loop 只扩 RW 层，只扩不缩；内层非 ext/f2fs 时拒绝。尾部 RW 层尚未格式化（首启安装现场）不算失败：分区照扩，该层由首次挂载时的 fstools 初始化，本次无事可做。多设备 btrfs 拒绝扩容。

## License

MIT（见 `LICENSES/`）