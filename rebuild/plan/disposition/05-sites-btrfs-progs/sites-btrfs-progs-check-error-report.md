# sites-btrfs-progs-check-error-report

## 判断
check 内部诊断/修复过程打印（132 行，全部裁剪）。全批已逐行翻态，note 见同名 tsv。
只读形态下输出不进决策；本实现只消费退出码，不复刻打印面。

## 依据
src/app/execute/diagnose.rs::run（退出码映射，stderr 只作消息附录）；src/platform/linux/tools.rs::check_arguments。

## 撤回条件
出现依赖 check 逐项诊断文本的外部契约，或本实现改自研检查器时重审。

## 换版重放补充（btrfs-progs 6.6.3→7.3）

换版重放在本批新增/改写 25 行（state=待判），已全部翻态：拒绝 0 / 裁剪 25；note 见同名 tsv。
- 判断：新增/改写行仍属 check 内部诊断与修复过程打印（含新 [1/8]–[8/8] 阶段进度打印、qgroup 报告输出），随外部工具自带；只读形态下输出不进决策，本实现只消费退出码。
- 依据：src/app/execute/diagnose.rs::run；src/platform/linux/tools.rs::check_arguments。
- 撤回条件：出现依赖 check 逐项诊断/进度文本的外部契约，或本实现改自研检查器时重审。
