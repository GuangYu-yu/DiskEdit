# 互操作：其他镜像格式、远程目标、稀疏镜像

以下流程都不由本工具实现，也不在本仓库的测试范围内。它们是"在工具之外完成，再把结果交给本工具"的配方。工具本身只处理 raw 文件与块设备；其他镜像格式由宿主呈现，工具从不读写格式本身。命令来自各自项目的文档；值得看的是接缝，因为它们说明本工具会与不会注意到什么。

## qcow2 / vmdk / vdi 镜像

宿主可以把镜像呈现为 raw 文件，工具随后像普通镜像一样打开它。QEMU 通过 FUSE 导出块节点（QEMU 6.0 及以上，需要 `/dev/fuse`）；`driver=` 指定格式：

```bash
qemu-storage-daemon \
 --blockdev driver=file,node-name=file,filename=disk.qcow2 \
 --blockdev driver=qcow2,node-name=qcow2,file=file \
 --export type=fuse,id=export,node-name=qcow2,mountpoint=disk.raw,writable=on
# 对 disk.raw 运行本工具
```

- 前提：QEMU 6.0 或更新且构建包含 FUSE 导出，以及可用的 `/dev/fuse`。挂载点必须是普通文件；导出期间它被遮蔽，自身内容不被改动。挂载存在之后再打开它——更早打开的句柄仍会看到文件原本的内容。
- 导出不需要 root：daemon 与工具只需要镜像文件和 `/dev/fuse`。
- 命令会扩展镜像时加 `growable=on`：写到当前末尾之后会增长块节点；不加则这类写入失败。
- 镜像格式需要可写的 block driver。qcow2 可写；vmdk 与 vdi 取决于构建，此处未实测。
- FUSE 视图是单个文件，没有分区节点；本工具按偏移访问分区，所以不需要。
- 文件系统层命令仍需要 root：它们经 `losetup` 到达分区，与 raw 镜像相同。
- 对本工具而言，镜像的身份是文件的 `(st_dev, st_ino)` 与大小。同一路径上重建的 FUSE 挂载是另一个 inode，因此一次挂载期间写下的记录在重新挂载后找不到：操作崩溃时按备份恢复镜像，不要指望记录。把 raw 视图以固定 serial 作为 virtio-blk 磁盘交给 guest（见免 root 一节）时，身份改由该 serial 提供。
- qcow2 镜像可以引用 backing file。overlay 记录该路径，每一层都必须存在且可访问，导出期间不能有其他进程修改其中任何一层；启动前用 `qemu-img info --backing-chain disk.qcow2` 列出整条链。
- 导出期间不要让其他进程、VM 或镜像工具修改该镜像。QEMU 的警告是明确的：修改正在使用的镜像可能销毁它，查询正在变化的镜像可能得到不一致的状态。
- 容器格式的解析（`qcow2`、`vmdk`、`vdi`）发生在宿主侧的 QEMU 进程里。免 root 配方隔离的是文件系统层，不是这个解析器；来源不可信的镜像属于宿主的输入。
- 内核 nbd 设备（`qemu-nbd -c /dev/nbd0`）同样把镜像呈现为块设备，但该设备没有能在重建后仍然成立的身份，所以所有写表命令都会被拒绝（退出码 6）。它适合只读。
- 分区节点（`/dev/nbd0p1`）只在内核扫描设备后出现；工具之外发生改动后，使用分区节点前需要再次运行 `partprobe /dev/nbd0`。
- 容量与扇区大小来自 nbd，而不是 qcow2/vmdk/vdi 元数据；没有可传 `--sector-size` 的地方。
- 分离必须成功才算映射结束。`qemu-nbd -d` 失败时可能残留块设备或连接；复用该名字前检查 `lsblk` 与 `/sys/block/nbd*`。

## 把磁盘导出为 qcow2

```bash
qemu-img convert -f raw -O qcow2 disk.img disk.qcow2
```

- `-f raw` 明确源格式，而不是让工具去探测。
- `-n` 跳过创建目标卷；目标必须已经存在。
- `-c` 压缩输出；适用于 qcow/qcow2 以及 `streamOptimized` 子格式的 vmdk。
- `-C` 是复制卸载，不是压缩：它可能加快从远程存储的转换，并可能让目标完全分配。

## 稀疏与预分配镜像

```bash
qemu-img create -f qcow2 -o preallocation=metadata disk.qcow2 20G
qemu-img convert -f raw -O qcow2 disk.img disk.qcow2
```

- `preallocation=metadata` 只预留 qcow2 元数据。qcow2 的模式有 `off`、`metadata`、`falloc`、`full`，区别在于 guest 写入之前宿主先分配了什么。
- 稀疏分配影响的是宿主存储占用，不是 guest 看到的磁盘内容。
- 恢复记录的大小取决于工具记录的字节区间，不取决于镜像已分配的容量；不要由一方推断另一方。
- `copy` 是分区级复制，不产生整盘文件，因此整盘镜像的分配策略留在工具之外。

## 远程镜像与远程磁盘

```bash
rbd map pool/image
nbd-client <host> <port> /dev/nbd0
```

- `rbd` 与 `nbd` 设备没有本工具能从设备接口建立的身份：既没有 WWN、serial，也没有可重新推导的稳定名字。所有写表命令都会被拒绝（退出码 6）。这是工具身份模型的限制，不是 NBD 或 RBD 的固有属性：两种协议都能给导出一个名字，但工具无法拿这个名字去核对映射后的设备。
- NBD 定义了显式的 FLUSH，但持久性取决于客户端是否发送、服务端是否转发、后端存储是否兑现。`qemu-nbd` 默认 `--cache=writeback`，不是写穿；不核对整条路径，就不能把一次成功的 `fsync` 当作持久性的证明。
- 本地 `flock` 不提供分布式互斥：两台主机，或同一导出的两个别名，各自拿到的是独立的锁。远程目标需要一把作用域与实际写者一致的锁。
- sshfs 或 NFS 上的 `flock` 必须在实际部署中确认后才能依赖。

## 在 QEMU microvm 中免 root 运行

文件系统层命令经 `losetup` 到达分区，因此需要 root。没有 root 时，把同一个二进制放进 guest，用 guest 里的 root 完成工作；二进制本身不变。QEMU 的 `microvm` 是 x86 上最小的机型：没有 PCI、没有 ACPI，最多八个 virtio-mmio 设备，且只能以 `-kernel`/`-initrd` 启动。

- x86 上没有更小的机型。`pc` 与 `q35` 带 PCI、ACPI 与固件；`isapc` 没有 virtio-mmio；`nitro-enclave`、`xenfv`、`xenpv` 面向特定宿主。arm64 上的机型是 `-M virt`：它同样提供 PCIe，所以 virtio-pci 也可用，但下面的 virtio-mmio 设备在那里同样成立。
- 镜像以 virtio-blk 磁盘挂入。它的 serial 是 guest 能看到的唯一稳定身份，因此传入由宿主算出的键。serial 最长 20 个字符，更长的值会被静默截断：键要由镜像内容派生而不是由路径派生，不同镜像不能落到同一个键，在 guest 内读取该属性而不是假定 sysfs 路径，并在启动 QEMU 之前拒绝过长或碰撞的键，不要依赖截断。
- 状态目录经 virtio-9p 挂入。只用 `-virtfs` 会创建一个 microvm 无法使用的 PCI 设备，所以要显式命名设备。这种显式写法在两种架构上都成立。`security_model=none` 不做属主映射：文件按 QEMU 进程的凭据创建，实际属主需要在部署环境里确认。
- 保留 ISA 串口：`-serial stdio` 把控制台接到它上面。`isa-serial=off` 只适用于改用 virtio console 作为控制台的 guest。
- `x-option-roms=off` 可以安全传入。`pit=off`、`pic=off`、`rtc=off` 不属于基础配方：关闭 PIT 与 PIC 要求 guest CPU 支持 `TSC_DEADLINE`，而 `rtc=off` 让 guest 只依赖 `kvmclock`，下面的 TCG 模式提供不了它。

```bash
qemu-system-x86_64 -M microvm,x-option-roms=off \
  -nodefaults -no-user-config \
  -display none -serial stdio -monitor none \
  -accel kvm -cpu host -m 256M -smp 1 \
  -kernel bzImage -initrd initramfs.cpio.gz \
  -append "console=ttyS0,115200 quiet reboot=t" \
  -drive id=target,file=disk.img,format=raw,if=none \
  -device virtio-blk-device,drive=target,serial=0123456789abcdef0123 \
  -fsdev local,id=state,path=/var/lib/diskedit,security_model=none \
  -device virtio-9p-device,fsdev=state,mount_tag=state \
  -no-reboot
```

- `-accel kvm` 不会自动回退。`/dev/kvm` 不存在或当前用户不可写时，把 `-accel kvm -cpu host` 换成 `-accel tcg -cpu max`；guest 会慢数倍，命令其余部分不变。判断方式应是实际打开 `/dev/kvm`，而不是只看文件是否存在。

arm64 上的机型是 `-M virt`，控制台是 PL011：

```bash
qemu-system-aarch64 -M virt \
  -nodefaults -no-user-config \
  -display none -serial stdio -monitor none \
  -accel kvm -cpu host -m 256M -smp 1 \
  -kernel Image -initrd initramfs.cpio.gz \
  -append "console=ttyAMA0,115200 quiet" \
  -drive id=target,file=disk.img,format=raw,if=none \
  -device virtio-blk-device,drive=target,serial=0123456789abcdef0123 \
  -fsdev local,id=state,path=/var/lib/diskedit,security_model=none \
  -device virtio-9p-device,fsdev=state,mount_tag=state \
  -no-reboot
```

- guest 内核在两种架构上都需要：`CONFIG_VIRTIO_MMIO`、`CONFIG_VIRTIO_BLK`、`CONFIG_NET_9P_VIRTIO`、`CONFIG_9P_FS`。控制台驱动不同：x86 是 8250 系列，arm64 是 `CONFIG_SERIAL_AMBA_PL011`。initramfs 需要：静态二进制、命令所需的文件系统工具，以及一个挂载 `state` 并执行该二进制的 init。
- 关机与退出状态：init 先同步、把程序的退出状态写到 9p 共享上的文件，再结束 guest。x86 上传 `reboot=t`：强制重启会变成 triple fault，`-no-reboot` 让 QEMU 随之退出。arm64 没有 `reboot=t` 这条路径；关机用 PSCI `SYSTEM_OFF`，不要把 `SYSTEM_RESET` 当作成功关机。两条路径下 QEMU 的退出行为要分别验证。QEMU 自己的退出状态说明的是 VM 如何结束，不是程序返回了什么；包装器读取状态文件并返回那个值。
- 身份：记录目录由 guest 看到的设备身份派生，与宿主对同一镜像的视角不同。guest 里写下的记录只能沿那条路径续跑；宿主侧的 `records` 与 `inspect` 不会列出它们。把 guest 当作独立命名空间。
- 锁：启动 QEMU 之前先在宿主拿到目标锁，guest 退出后释放。锁必须覆盖原始镜像，而不只是它前面的 FUSE 视图；guest 内部拿到的锁不能排斥宿主进程。
- 持久性：把状态写入临时文件，`fsync` 该文件，rename，再 `fsync` 目录，然后才结束 guest。要确认这两次 `fsync` 经 9p 到达宿主存储；系统调用成功与原子 rename 本身都不能证明持久化。
- 信号与输出流：终止 QEMU 只是结束 VM，并不会把同一个信号投递给 guest 里的程序。要转发信号，或要把 stderr 从 stdout 分离，需要另一条通道（例如 virtio-serial 端口）；串口控制台只有一条流。

## 对损坏卷强制 NTFS resize

没有一个能总是跳过文件系统步骤的模式：`resize --end` 与 `resize --grow-to-end` 会执行与尺寸变化匹配的文件系统步骤（变大则增长，变小则缩小），除非给出 `--no-fs`。`--no-fs` 本身在缩小时被拒绝（新的末端会切进尚未缩小的元数据），在结束一个已扩展但文件系统步骤仍待办的分区时也被拒绝。`recover --replay-table-only` 是独立的纯表恢复路径：它回放一次数据已经搬移完成的 `move` 所记录的表区域，不恢复这些区域里的数据。因此手动执行 `ntfsresize --force` 之后再跑一次普通的 `resize --end` 会再次运行 `ntfsresize`；要保留文件系统不动时传 `--no-fs`。本工具以 `ntfsresize -f -f`（增长，先做一次 `--no-action` 演练）和 `ntfsresize -f -f -s <bytes>`（缩小）调用它，所以卷的 dirty 标记本身不会阻止该步骤。

这组组合是否安全尚未运行；按未验证对待。`--force` 绕过一致性检查的守卫，并不修复文件系统损坏。强制 `ntfsresize` 之后，卷会被标记为需要检查：Windows 可能请求或安排一次检查，应先运行 `chkdsk` 并核对卷，再正常使用。

## 所有配方共同遵循的边界

- **独占访问。** 这些配方都不能让并发写者变安全。导出、挂载或 guest 存活期间，不要让其他工具、VM 或 `qemu-img` 命令写同一个镜像；QEMU 的警告是明确的：这可能销毁镜像。
- **解析器信任。** 把文件系统工作放进 guest，只隔离了文件系统层。容器格式由宿主进程解析，因此来源不可信的镜像属于宿主的输入。
- **备份。** 恢复记录覆盖的是本工具记录过的字节区间，不覆盖文件系统内部损坏、镜像格式元数据、缺失的 backing 层，也不覆盖断电留下的多处不一致。无法重建的东西应当有备份。