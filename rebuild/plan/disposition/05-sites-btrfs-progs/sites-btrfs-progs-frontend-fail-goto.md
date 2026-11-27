# sites-btrfs-progs-frontend-fail-goto

## 判断
cmds 层失败清理跳转（68 行：60 裁剪 + 8 拒绝）。全批已逐行翻态，note 见同名 tsv。
- 裁剪 60 行：df/show/sync/scan、defrag、mkswapfile、filesystem usage、property、device 级子命令——绑定未消费子命令（device/balance/子卷/raid 级超出本实现范围，档案表只登记 resize/check/attrs/mkfs）。
- 拒绝 8 行（00034/36/37/39/41/43/45/47）：check_resize_args 的 goto out——C 控制流载体，校验语义已在同函数 refusal 行按 SA2-006/007/008/016 认领；Rust 错误类型化传播，逐点复刻属多虑。

## 依据
SA2-006/007/008/016；crates/core/src/fs/catalog.rs（消费面登记）；plan/06-防漂移执行手册.md B17（多设备 btrfs 占用证据）。

## 撤回条件
档案表登记 device/子卷/raid 级能力或提供相应子命令消费时重审裁剪行；resize 校验语义被推翻时重审拒绝行。
