# 行为边界与限制

README 只保留"它是什么、怎么用"。本文件记录行为约束：能力闸门、占用与并发判定、目标身份、恢复记录、续跑语义、外部工具与验证范围，以及与标志注册表核对过的选项表。这些行为发生变化时同步更新本文件。

## 平台能力闸门

每条命令的结果、闸门评估的目标形态以及进程级退出时打印的文本都由[能力矩阵](capability-matrix.md#runtime-gate)生成；本节只说明检查的顺序。闸门在几何计算、锁获取与任何伴生文件创建之前运行，也在影响确认闸门之前运行。缺 `--yes` 的用法检查更早：它在参数解析内、分派之前运行，并以用法错误退出。闸门只遍历平台报告为可打开的目标形态；不在该集合中的目标形态不由这个闸门评估。

## 执行放置

写入只有一条路径：一份作业是一串载体步骤（`TableCommit`、`Transfer`、`Fs`、`Probe`），宿主进程是恢复记录、锁与输出的唯一写者。步骤在哪里执行属于部署细节，每份作业在首次写盘之前决定一次，步骤之间不再改变。

- **原生**：宿主满足本作业所需的全部能力——文件系统层要求的平台/root 权限，以及步骤表解析出的每个外部工具。这份清单与原生执行器读的是同一份工具矩阵。
- **VM**：宿主能力不足，但 `DISKEDIT_VM_BUNDLE` 指向一个通过校验的载体 bundle。整份计划移入 guest；记录、锁、续跑游标与退出码仍由宿主掌握。
- 两者都不满足：以退出码 6 退出，给出缺口与两条出路（安装工具，或提供 bundle）。

bundle 目录包含 `manifest.json`、`kernel` 与 `initramfs`。清单声明 `schema`、`arch`、`binary_version`，并为每件制品给出相对 `path` 与 `sha256`；可选的 `worker.sha256` 记录 worker 映像摘要供审计尾段使用。只有以下全部成立才接受：schema 是当前版本、架构与本进程一致、`binary_version` 与本可执行文件版本一致、每件制品的路径都落在 bundle 目录内、每个 SHA-256 都对得上。任何一项不符都在启动 QEMU 之前拒绝——宿主不会把目标交给身份不明的 guest。

启动器把已打开的目标经文件描述符交给 QEMU（`-add-fd` 加 `/dev/fdset/`），通过 `-blockdev` 交给块层、缓存固定为 `cache.direct=on`，以 virtio-blk 按目标的逻辑扇区大小（512 或 4096）呈现，协议走串口控制台，熵由 virtio-rng 提供。QEMU 自身的退出状态只描述虚拟机如何结束；某一步是否落盘由协议的 `result` 与宿主的探测回答，不看该状态。

## 占用边界

对块设备的写入依赖以 `O_EXCL` 打开整块磁盘：只要有分区处于挂载状态或用作 swap，内核就以 EBUSY 拒绝打开；写入前还有第二道占用闸门再检查一次（在块设备上逐个探测分区——纵深防御）。镜像文件没有这种内核语义，因此闸门反向枚举 loop 设备：把已打开的镜像句柄的 `(st_dev, st_ino)` 与 `LOOP_GET_STATUS64` 返回的 `(lo_device, lo_inode)` 比较；匹配时检查该 loop 及其分区是否已挂载（遍历每个可读的挂载命名空间）、是否为活动 swap、是否被块设备栈持有。判定不依赖 `loop/backing_file` 路径，因此硬链接别名、已删除的 backing 路径以及其他挂载命名空间中的挂载都能被发现。本进程登记的 loop 排他性是进程级名字表；每个条目被消费时都对照当前活动的映射重新检查（名字只用于定位登记项）。占用判定匹配每个 backing 具有该文件 `(st_dev, st_ino)` 的 loop，因此它回答的是"该文件已被映射或被持有"，而不是同一文件系统上多个叠加映射中哪一个是它。**直接写表的命令**（`new`/`add`/`delete`/`set name|type|flag`/`recover`）要求目标完全空闲：任何分区在使用都会使新表与内核视图分叉，因此该命令不关心它编辑的是哪个分区。`copy` 是例外——它只在重叠检查之后向空闲区域添加一个条目，不改动任何既有区间，也不移动任何分区的区间。移动类命令（`resize` 的每种形式——`--end`、`--grow-to-end` 或给定尺寸——以及 `move`）与 `mkfs` 按写入的形态区分：保持起点、只延长末端的写入在仅被 dm 持有的目标上允许（LVM PV 与 dm-crypt 的增长语义）；移动、收缩与整段重写一律拒绝。若任何被枚举的 loop 节点无法打开、ioctl 返回 ENXIO 以外的错误、sysfs 无法读取，或因权限原因无法读取 `/proc/<pid>/mountinfo`（例如 `hidepid`），工具拒绝写入（退出码 4），按"无法确认"处理——即使该 loop 与目标无关；这是安全优先的环境限制，不是目标正在被使用的证据。

## LVM 收尾边界

镜像内 PV 的 `pvresize`/`lvextend` 通过临时 loop 映射执行。如果该 PV 已被另一个 loop 映射且 VG 有活动 LV，LVM 更倾向于设备视图"in use by an LV"而不是临时映射，因此针对临时映射的 `pvresize` 不生效（退出状态仍为 0）。工具从后置条件检查报告退出码 5（后置条件视图陈旧）并打印补救办法；它不会宣称成功。分区表扩展已经提交；PV、LV 与文件系统未变。要在该配置下走完 LVM 链，先停用 VG、释放用户侧 loop 再重新运行。工具不会提升由另一层拥有的 loop 的 `sizelimit`：当外部 loop 的偏移与请求相同但 `sizelimit` 不同（更高或更低）时，它拒绝，并指向从拥有该映射的层解绑再重新绑定（`losetup -d <dev>`，让该栈重新映射）；`--grow-lv` 同理——没有这次重新绑定，拥有者不会替你走完 LVM 链。

对镜像内 PV 成功执行 `--grow-lv` 之后，`lvextend -r` 会让此前处于非活动状态的 LV 保持活动，因此临时 loop 无法立即解绑：`losetup -d` 返回成功，内核把该 loop 标记为 autoclear，映射在 VG 停用时（`vgchange -an`）才被释放。这是临时映射的生命周期，不是泄漏；VG 停用后，`losetup -a` 不再列出该镜像。

## 并发边界

对目标的排他锁是*建议性*的：它只约束遵循 DISKEDIT 协议的进程，且只在它们的各次调用之间生效。**它不是针对系统上每个写者的强制锁**，不会阻止其他程序、内核侧写入或绕过本工具的路径写入同一目标；不得把它读作系统级的排他保证。整盘 `O_EXCL` 与挂载占用闸门只在内核语义内阻止写者。镜像的 loop 反向查找是写入前的时点判定：在检查与实际写入之间，外部进程仍可创建 loop、启动 swap 或挂载该镜像，而本工具的锁不约束这些进程。写表在解析时读取一次表，并在提交时把该快照写回；提交前它重新读取条目数组并逐条与快照比较（本层编辑过的槽位可能已等于目标值），任何条目不同就拒绝写表，因此外部进程在窗口期内所做的改动不会被静默覆盖（修复步骤使用同一规则，覆盖添加、设置与删除，以及整条重定位与 resize 路径）。要与其他写者共享目标，请在外部串行化。inode 级第二凭据仅在 Unix 上获取；非 Unix 平台只承诺锁文件。硬链接、符号链接与 junction 别名各自获取自己的锁、各有各的场景；经别名并发使用不在支持边界内（`std::fs::File::try_lock` 在 Windows 上是强制性的整文件锁，不能代替该凭据）。

## 设备身份边界

块设备的恢复记录位于 `<state_root>/recovery/<identity-hash>/record.bin`——目录名是按顺序拼接身份字段（设备类别、稳定身份字符串、载体 `file_id`）后取 sha256 的前 16 字节，小写十六进制；分区目标的稳定身份字符串是载体的稳定字符串加上分区号，因此分区节点与整盘节点各有自己的记录目录。锁不按目标派生：整盘的锁位于 `<state_root>/<disk key>.lock`，两者共用一个锁。状态根默认为 `/var/lib/diskedit`；在 Linux 上，当 euid 不为 0 时忽略 `DISKEDIT_STATE_DIR`；其他平台没有块设备目标，使用镜像侧状态目录。稳定身份取 `wwid` → `device/wwid` → `device/serial` 中第一个非空的原始文本（最后一项仅在两次读取一致时采用）。`dm/uuid` 与 `md/uuid` 不参与记录身份：两者描述的都是用户态创建后才存在的映射或已组装阵列，重启之后、记录正待续跑之时无从重读。dm、md 目标的记录身份因此来自 `--identity NAME`（记录键 `declared:<NAME>`）；只有 major:minor 的目标、不可读的目标、两次读取不一致的目标，以及未给名字的 dm/md 目标为 `Unusable`——没有记录目录，写命令以退出码 6 拒绝。当 sysfs 拓扑（节点、`partition` 属性、父设备）无法读取时，工具同样拒绝，不会退回到由调用方路径构造的 `{devname}-{capacity}`。容量不参与分区身份。镜像身份由路径派生：它标识"该路径"，而不是"该文件"。在 Linux 上，realpath 只合并符号链接与路径写法，**不合并硬链接**：同一 inode 的两个硬链接名派生出两个记录目录（`<image_state_root>/images` 下两个不同的 `<key-hash>` 目录），经名字 A 中断的任务经名字 B 不可见，也无法从 B 续跑或撤销——此类任务必须在同一名字下完成。锁有两层：**锁文件**（块设备：整盘为 `<state_root>/<disk key>.lock`，与其分区共享；镜像：`<image_state_root>/images/<key-hash>/record.lock`，当不存在按用户的状态根时为 `<image>.diskedit/record.lock`；loop：backing 文件旁的兄弟锁文件；disk key 在存在 device-id 时为该值，否则为 sysfs 父节点名加上以扇区计的容量）与 **inode 级第二凭据**（仅 Unix：对 `data_path` 新打开的句柄加锁——镜像为数据文件，块设备为设备节点）。后者把同一 inode 的别名路径收敛到一把锁；整盘与其分区由共享的锁文件收敛。非 Unix 平台只承诺锁文件；硬链接、符号链接与 junction 别名各自获取自己的锁、各有各的场景，经别名并发使用不在支持边界内（`std::fs::File::try_lock` 在 Windows 上是强制性的整文件锁，不能代替）。

## 恢复场景的身份检查

恢复记录还记录创建时的目标（`SceneIdentity`）：身份材料（块设备为 device ID，镜像为 file ID）、证据类别、载体容量、loop 映射（偏移与 sizelimit）以及载体文件身份；回放或续跑之前逐字段比较，任何**可比较字段**不同即拒绝。证据类别是身份字段：为某一类别写下的记录不会匹配报告为另一类别的目标。File ID 在 Unix 上为 `(st_dev, st_ino)`，在 Windows 上为卷序列号加文件索引；卷序列号是画像字段，不参与身份判定。容量与载体文件 ID 仅在双方都提供时比较；缺失项不参与。loop 映射只要任一侧存在就参与：没有的一侧计为不同，因为经 loop 写下的记录不得对同一文件在不带映射打开时回放。仅凭容量不能标识目标：同样大小的替换文件仍可能通过。完全没有稳定身份（`Unusable`）的目标在**写入任何字节之前**就被拒绝：这样的场景既无法续跑也无法撤销，因此死路不能在恢复时才被发现；留下"无证据"的记录永远不匹配任何当前目标，回放前的检查以退出码 3 并附原因拒绝它。

## 记录中的磁盘 GUID 凭据

除身份段之外，记录（magic `DRE2`，16 字节头部由 magic、载荷长度、载荷 CRC 与头部 CRC 组成——没有单独的版本字段）以三种状态保存目标的 GPT 磁盘 GUID：`Present` / `Unavailable` / `GeneratedThisOperation`。`Present` 必须与磁盘一致；`Unavailable` 表示目标上没有可读的 GPT（MBR / 未分区），不构成证据；`GeneratedThisOperation` 只出现在记录侧（整张表由写入该记录的操作生成），不与磁盘 GUID 比较。两侧均为 `Present` 但值不同、记录为 `Present` 而目标没有可读 GPT、记录为 `Unavailable` 而目标现在带有 GPT，都是身份不匹配。载荷末尾是审计尾段——放置（`native` 或 `vm`）、可选的载体 worker 摘要，以及本作业所需外部工具的版本行——在首次写盘之前写一次，续跑与回滚都不读它。**一种 magic 一种布局**：magic 不是 `DRE2` 的记录按无法识别的格式报告（`UnrecognizedFormat`，携带 magic 字节）；本次构建不回放也不迁移它，会写盘的命令以退出码 3 拒绝。因此旧版本写下的记录无法续跑：用写下它的那个版本完成，再用 `recover --release` 清除。`recover --release` 的自动隔离要求探测能解码整条记录（magic、头部 CRC、载荷 CRC 以及每个尾部段），且记录的身份属于当前载体。

## 恢复记录的位置

恢复记录是其位置上的一类普通文件（镜像为 `<image_state_root>/images/<key-hash>/record`，块设备为 `<state_root>/recovery/<identity-hash>/record`；镜像侧的 `<image>.diskedit/record.lock` 在不存在按用户的状态根时只是锁，其中不含记录）。该位置上是什么由一次探测回答：不存在、可解码、已损坏（部分写入或 CRC 不匹配）、无法识别的格式（magic 不是 `DRE2`）、不是恢复文件（符号链接 / 目录 / 非普通文件），或不可读。在 Linux 上，路径以父目录为锚点，用连续的 `openat(O_NOFOLLOW|O_DIRECTORY)` 解析，最后一级用 `O_NOFOLLOW`：任一级为符号链接、非目录，或最后一级为非普通文件，都算"不是恢复文件"。非 Linux 是保留的移植位：它只用 `symlink_metadata` 回答存在性，从不读取内容，因此那里的普通文件按"不可读"拒绝——在该平台上本次构建可以拒绝，但无法读取记录。删除与重命名作用于该路径条目本身（父目录 fd 加相对名字），不跟随链接、不改变链接目标；原子替换是同目录内的重命名。当条目不是恢复文件，或删除失败时，场景被视为仍然存在，工具不会报告其已清理。

## Windows 上的目录项持久性

删除或重命名恢复文件之后，工具不宣称目录项已到达磁盘；它只打印 `note: removed <path>; persistence of the directory entry was not verified on this platform`（重命名同理）。该平台对目录项不提供掉电持久性保证，因此这条提示不得读作"已经持久化"。

## 续跑边界

重新运行原命令会从其恢复记录续跑，适用于 `new`、`move`、`resize`（每种形式：`--end`、`--grow-to-end` 或给定尺寸）、`mkfs`、`resizefs` 与 `set`；GPT 与 MBR 共享同一记录族，因此中断的 MBR 扩容以同样方式续跑，`recover` 会把记录中的表写入回滚。在线 resize（块设备且分区已挂载）改为经由工具写入：没有可续跑的内容，`recover` 也无法回滚。`add`、`delete` 与 `copy` 没有续跑路径（`help copy` 注明 `No resume`），因此重新运行前先用 `recover` 回滚，或用 `recover --release` 释放场景。收缩步骤记录三种状态（未收缩 / 已验证 / 未验证）：当文件系统大小无法读回时，任务在收缩步骤停下，并以退出码 5 报告未完成的事务，重新运行时先读取实际大小，确认收缩从未发生后才再次执行。ext2/3/4、ntfs 与 btrfs 的扩容会读回；f2fs、xfs 与 vfat 没有读回，因此成功的扩容仍为未验证：该次运行打印一条点名尺寸检查的待办项并以 5 退出。结束时只有在六个条件全部成立才删除恢复记录：记录可识别、其提交状态为已提交、身份匹配、持久性已确认、后置条件读回通过，且表写入步骤已完成。否则保留记录，并打印 `the recovery record was not cleaned up (<reason>) — release it with diskedit recover --release`。删除本身失败时，事务以退出码 5 报告，而不是成功。搬移的数据按批次同步并推进持久游标（同步先于记录推进，游标永不超前于实际落盘的字节）；批次大小随提交延迟自适应，因此崩溃后最多重做 256 MiB 的已复制数据，大于该窗口的块尺寸退化为逐块提交。自我重叠的搬移（位移小于源区间）在所有路径上都逐块提交——按批次缓冲的块会伸入重做区间，而重做会读到被同一批次覆盖的源数据。

## 外部工具

只在该步骤需要时查找；它们不必全部存在：

| 用途 | 工具 |
|---|---|
| 内核同步 / 在线写表 | `sfdisk`, `partx` |
| loop 与挂载 | `losetup`, `udevadm`, `mount`, `umount` |
| ext2/3/4 | `mke2fs`, `e2fsck`, `resize2fs`, `dumpe2fs`, `tune2fs` |
| xfs | `mkfs.xfs`, `xfs_growfs`, `xfs_repair`, `xfs_admin` |
| btrfs | `mkfs.btrfs`, `btrfs`, `btrfstune` |
| ntfs | `mkfs.ntfs`, `ntfsresize`, `ntfsfix`, `ntfslabel` |
| f2fs | `mkfs.f2fs`, `fsck.f2fs`, `resize.f2fs` |
| vfat | `mkfs.vfat`, `fsck.vfat`, `fatresize`, `fatlabel` |
| exfat | `mkfs.exfat`, `fsck.exfat`, `exfatlabel` |
| swap | `mkswap`, `swaplabel` |
| LVM | `pvs`, `lvs`, `vgs`, `pvresize`, `lvextend` |

每次外部调用都带 `LC_ALL=C`，因此本地化输出不会破坏解析。

## OpenWrt 组合布局

同一分区内是只读的 squashfs/erofs 根加上尾部的 RW overlay：扩容时识别内层文件系统，并仅通过偏移 loop 扩展 RW 层，且只扩容；内层文件系统不是 ext 或 f2fs 时拒绝。尚未格式化的尾部 RW 层（首次启动安装场景）不算失败：分区照常扩容，该层在首次挂载时由 fstools 初始化，此处无事可做。多设备 btrfs 的扩容被拒绝。

## 验证范围

工具以 Linux `util-linux` 和各文件系统官方工具的命令行契约为准，不绑定某个发行版；回归套件（`tests/wsl/`）在 Ubuntu 上以 `util-linux` 2.41.3 验证过，其他发行版与版本未验证。占用探测记录在下面的等级中；未运行过的路径不会被描述为通过：

| 证据等级 | 内容 |
|---|---|
| 已运行并验证 | loop 身份编码（`st_dev`/`st_ino` 对照 `lo_device`/`lo_inode`）、未绑定 loop 的 `ENXIO`、硬链接别名与已删除的 backing 路径、其他挂载命名空间中的挂载、loop 与分区挂载、活动 swap、sysfs holders、已退出进程的 mountinfo（`EINVAL`）、失败即拒绝的探测（退出码 4）、在没有持有者的 LVM PV 上完整的 `--grow-lv` 链、活动 LV 持有分区时 `AllowDm` 不被通用规则拒绝（链到达 LVM 步骤）而整段 `mkfs` 重写仍被拒绝、在已挂载镜像上 `new`/`add`/`delete`/`set name\|type\|flag`/`recover` 全部被拒绝且磁盘字节不变、多个 loop 匹配时点明实际持有者的拒绝、非 root 用户可用的镜像操作（该用户无法打开 `root:disk` loop 节点） |
| 未验证 | 容器内独立 PID 命名空间下 loop 枚举的完整性；因 `hidepid` 等权限限制导致 mountinfo 不可读的部署（代码按探测失败拒绝，但未在真实 `hidepid` 环境中运行过）；上表以外的发行版与内核版本；**镜像内 PV 被另一个 loop 映射且 VG 有活动 LV 时完整的 `--grow-lv` 收尾链**（见上一节：LVM 的重复 PV 规则使针对临时映射的 `pvresize` 成为空操作，工具报告退出码 5 而不宣称成功） |
| 设计限制 | 占用闸门是写入前的时点判定，不阻止检查之后创建的外部 loop / swap / 挂载；镜像侧枚举只覆盖本进程可见的 loop 节点，因缺少权限（`root:disk`）而无法打开的节点按未绑定跳过——创建绑定与访问节点需要同一权限类别，因此这类用户无法为该镜像创建绑定；不存在的节点、`ENODEV` 以及其他无法确认身份的情形按探测失败拒绝，该边界随内核/udev 的权限模型移动；失败即拒绝同样适用于无关 loop 的探测失败；缺失的记录项（`None`）不参与比较 |

## 选项

命令接受哪些选项由 `diskedit help <CMD>` 定义；下表列出每个选项及其取值形式。取值列写作 `—` 的选项不取值。

| 选项 | 取值 | 含义 |
|---|---|---|
| `--align` | A | 放置对齐：`mib` 或 `none`（默认为 `none`，即给定时所用的 LBA；由 `add`、`move`、`copy`、`resize --end` 与 `resize --grow-to-end` 接受；对 `resize` 作用于计算出的末端） |
| `--allow-move` | — | `resize` 在 GPT 上可以搬移阻碍操作的分区 |
| `--chunk` | MiB | `move`、`copy` 与 `resize` 的恢复粒度（MiB，1..=64）。续跑的任务省略该选项时沿用记录中的值，新任务使用 4；显式值与记录不同则拒绝。路径无法搬移时拒绝：MBR 的 `resize`、在线 `resize` 与 superfloppy 的 `resize` |
| `--discard-replay` | — | 当记录仍含有可回放区间（或载荷已损坏，或条目数未知）时，`recover --release` 必须给出该选项；仅 `recover --release` 接受 |
| `--dry-run` | — | `resize` 打印算出的范围、文件系统步骤与任何搬移计划，然后在第一次写入之前停下；不建记录，也不需要 `--yes` |
| `--end` | LBA | `resize --end` 的结束 LBA |
| `--grow-lv` | — | `resize` 在 LVM PV 上继续进入 LV 及其文件系统 |
| `--grow` | — | `info --grow` 在同一份 JSON 报告中增加 `preview` 键：`:N` 的搬移预览（不写入任何内容，也不产生可复用的计划） |
| `--grow-to-end` | — | `resize --grow-to-end` 把末端固定在可用空间的末尾 |
| `--identity` | NAME | 为不带 WWN、EUI 或可重复序列号的目标（`nbd`、`rbd`、`dm`、`md`）命名；所有写命令、`recover --release` 以及 `inspect` / `records list` / `records verify` 接受，只读命令用它定位与核对记录。续跑该记录、释放它或核对它都需要同一个名字；镜像与自带身份的设备拒绝该选项（退出码 2） |
| `--keep-partuuid` | — | `copy` 复用源 PARTUUID |
| `--label` | S | `mkfs` 写入的文件系统标签（省略时为空标签；不虚构默认值） |
| `--lv` | NAME | 配合 `--grow-lv` 要扩容的 LV（省略时为该 PV 上唯一的 LV） |
| `--name` | S | 分区名 |
| `--no-fs` | — | 仅 `resize`：跳过扩容分区的文件系统步骤 |
| `--overwrite-data` | — | `new` 授权覆盖落在新表元数据区域内的旧分区的字节 |
| `--part-number` | N | `add` 的条目槽位（从 1 起，必须空闲；省略时为第一个未使用的槽位） |
| `--random` | — | `set uuid` 要求目标生成新值：ext 用 `tune2fs -U random`，xfs 用 `xfs_admin -U generate`，btrfs 用 `btrfstune -u`；swap 没有生成形式，NTFS 序列号不是 Windows 卷 UUID |
| `--release` | — | `recover --release` 丢弃恢复文件并保留它们描述的写入；不给该选项时 `recover` 会继续或回放记录 |
| `--replay-table-only` | — | `recover` 回放记录中的表区域，这些区域来自一次目标与旧表所指区间重叠的 `move`；这些区间中的数据不会被恢复 |
| `--sector-size` | N | 镜像逻辑扇区大小（2 的幂，至少 512 字节；65536 字节上限是实现限制，不是格式要求）；对 `copy` 作用于一个目标 |
| `--start` | LBA\|end\|align | 起始 LBA；仅 `add`、`move` 与 `copy` 接受。对 `add` 可省略（能容纳 SIZE 的第一个空洞）；`move` 与 `copy` 必须给出，也接受 `end`；`align` 只有 `move` 接受（从当前起点向上取整到下一个 1 MiB 边界）。`resize` 只改变末端——搬移是 `move` |
| `--table` | gpt\|mbr | `new` 创建的分区表类型 |
| `--type-guid` | GUID\|0xNN | `add` 的分区类型：GPT 取标准 GUID 文本（省略时为 Linux filesystem data）；MBR 取两位十六进制类型字节（省略时为 0x83） |
| `--yes` | — | 写入前的确认闸门：`new`、`add`、`delete`、`move`、`copy`、`resize`、`mkfs`、`resizefs`、`set` 与 `recover` 要求给出，缺失时在解析阶段以退出码 2 退出；其他命令（`info`、`inspect`、`records list/verify/scan`、`check`）不接受 |
