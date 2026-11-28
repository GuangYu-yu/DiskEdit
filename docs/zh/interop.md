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
- 内核 nbd 设备（`qemu-nbd -c /dev/nbd0`）同样把镜像呈现为块设备，但该设备没有能在重建后仍然成立的身份：所有写表命令都需要 `--alias NAME`，回滚、回放与释放记录需要同一个名字；不给名字时写命令才退 6。名字是调用方给出的断言，不是从设备读出的事实。只读命令（`records show`、`records list`、`records verify`）接受同一个名字来定位与核对记录。
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

- `rbd` 与 `nbd` 设备没有本工具能从设备接口建立的身份：既没有 WWN、serial，也没有可重新推导的稳定名字。所有写表命令都需要 `--alias NAME`；回滚、回放与释放记录需要同一个名字，不给名字时写命令退 6。这是工具身份模型的限制，不是 NBD 或 RBD 的固有属性：两种协议都能给导出一个名字，但工具无法拿这个名字去核对映射后的设备。只读命令（`records show`、`records list`、`records verify`）接受同一个名字来定位与核对记录。
- NBD 定义了显式的 FLUSH，但持久性取决于客户端是否发送、服务端是否转发、后端存储是否兑现。`qemu-nbd` 默认 `--cache=writeback`，不是写穿；不核对整条路径，就不能把一次成功的 `fsync` 当作持久性的证明。
- 本地 `flock` 不提供分布式互斥：两台主机，或同一导出的两个别名，各自拿到的是独立的锁。远程目标需要一把作用域与实际写者一致的锁。
- sshfs 或 NFS 上的 `flock` 必须在实际部署中确认后才能依赖。

## 经 VM 载体免 root 运行

文件系统层的步骤需要 Linux 内核与 root。宿主缺少其中之一时，整份计划可以放进 VM 执行，而不是由人手工在 guest 里敲命令。放置是自动的，每份作业在首次写盘之前决定一次：宿主能执行的步骤留在原生路径；否则，若 `DISKEDIT_VM_BUNDLE` 指向的 bundle 通过校验，就交给 VM；两者都没有时，命令以退出码 6 退出，说明缺什么以及两条出路。校验规则见[内部实现](internals.md#执行放置)。

- 宿主的所有权不变。恢复记录、目标锁、续跑游标、协议会话与进程退出码都留在宿主进程。guest 只接收步骤、回报事件，并且只写目标。
- 目标以已打开的句柄交出，按目标的逻辑扇区大小作为 virtio-blk 接入。逻辑扇区既不是 512 也不是 4096 的目标在启动 QEMU 之前就被拒绝。
- 协议走串口控制台：以 `{` 开头的行是协议帧，其余行是诊断输出并转到 stderr。bundle 的内核命令行把控制台调成静默，因此工具输出与内核消息不会与协议帧交错。
- 同一二进制的 `__worker` 会话只用于协议测试，不是用户入口，也不出现在帮助、参考与能力矩阵中。

### 构建 bundle

bundle 是一个目录，包含 `manifest.json`、`kernel` 与 `initramfs`：

```json
{
  "schema": 1,
  "arch": "x86_64",
  "binary_version": "0.1.0",
  "kernel":    { "path": "kernel",    "sha256": "…" },
  "initramfs": { "path": "initramfs", "sha256": "…" },
  "worker":    { "sha256": "…" }
}
```

- `arch` 为 `x86_64` 或 `aarch64`；`binary_version` 是本可执行文件的版本。任一不符、任一 SHA-256 与文件不符，或制品路径为绝对路径、离开 bundle 目录，清单即被拒绝。
- initramfs 携带本可执行文件、guest 需要的文件系统工具，以及一个 init：它从内核命令行读取 `diskedit.token`，导出为 `DISKEDIT_WORKER_TOKEN`，再以 `__worker` 执行该二进制。内核需要 virtio-blk、virtio-rng 与对应控制台驱动（x86 为 8250，arm64 为 PL011）。二进制版本变化后要重建 bundle：worker 的版本必须与宿主一致。
- guest 回报步骤结果与屏障事件；宿主把记录落盘并在每个 checkpoint 之后回 ack，guest 才继续。QEMU 的退出状态说明 VM 如何结束，不是程序返回了什么。
- 经 9p 共享手工运行该二进制不是受支持路径。记录、锁与输出属于宿主；这样启动的 guest 会形成第二个命名空间。

## 对损坏卷强制 NTFS resize

没有一个能总是跳过文件系统步骤的模式：`resize --end` 与 `resize --grow-to-end` 会执行与尺寸变化匹配的文件系统步骤（变大则增长，变小则缩小），除非给出 `--no-fs`。`--no-fs` 本身在缩小时被拒绝（新的末端会切进尚未缩小的元数据），在结束一个已扩展但文件系统步骤仍待办的分区时也被拒绝。`DiskEdit replay` 是独立的纯表恢复路径：它回放一次数据已经搬移完成的 `move` 所记录的表区域，不恢复这些区域里的数据。因此手动执行 `ntfsresize --force` 之后再跑一次普通的 `resize --end` 会再次运行 `ntfsresize`；要保留文件系统不动时传 `--no-fs`。本工具以 `ntfsresize -f -f`（增长，先做一次 `--no-action` 演练）和 `ntfsresize -f -f -s <bytes>`（缩小）调用它，所以卷的 dirty 标记本身不会阻止该步骤。

这组组合是否安全尚未运行；按未验证对待。`--force` 绕过一致性检查的守卫，并不修复文件系统损坏。强制 `ntfsresize` 之后，卷会被标记为需要检查：Windows 可能请求或安排一次检查，应先运行 `chkdsk` 并核对卷，再正常使用。

## 所有配方共同遵循的边界

- **独占访问。** 这些配方都不能让并发写者变安全。导出、挂载或 guest 存活期间，不要让其他工具、VM 或 `qemu-img` 命令写同一个镜像；QEMU 的警告是明确的：这可能销毁镜像。
- **解析器信任。** 把文件系统工作放进 guest，只隔离了文件系统层。容器格式由宿主进程解析，因此来源不可信的镜像属于宿主的输入。
- **备份。** 恢复记录覆盖的是本工具记录过的字节区间，不覆盖文件系统内部损坏、镜像格式元数据、缺失的 backing 层，也不覆盖断电留下的多处不一致。无法重建的东西应当有备份。