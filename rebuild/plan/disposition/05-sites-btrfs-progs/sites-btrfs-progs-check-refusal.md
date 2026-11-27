# sites-btrfs-progs-check-refusal

## 判断
btrfs check 腐蚀检测/运行内拒绝面（250 行：248 裁剪 + 2 认领）。全批已逐行翻态，note 见同名 tsv。
本实现以固定参数 `btrfs check --readonly` 调用（crates/core/src/fs/catalog.rs check=Tools(BTRFS)、src/platform/linux/tools.rs::check_arguments），只消费退出码（src/app/execute/diagnose.rs::run）；I_ERR 旗标、inode/extent/chunk 校验与修复分支随外部工具自带，不逐点复刻。
认领 2 行（00975/00977）：check 运行前的占用/挂载闸（已挂载且无 --force 即拒绝、mount status 不可查即报错）——调用时生效的拒绝面。

## 依据
SA2-006/007/008/016 的消费边界（resize/回读已定案）；plan/baseline/capability-matrix.md（btrfs 检查=是）；src/platform/linux/tools.rs::check_arguments、src/app/execute/diagnose.rs::run。

## 撤回条件
本实现改自研 btrfs 检查器、固定参数不足以覆盖合法用法或上游改参导致 `btrfs check --readonly` 不可用时重审；出现依赖 check 文本输出的外部契约时重审；认领 2 行若检查器挂载语义变化（如上游移除 mounted+--force 闸）重审。
