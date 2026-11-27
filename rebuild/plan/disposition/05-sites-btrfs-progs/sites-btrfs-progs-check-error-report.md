# sites-btrfs-progs-check-error-report

## 判断
check 内部诊断/修复过程打印（132 行，全部裁剪）。全批已逐行翻态，note 见同名 tsv。
只读形态下输出不进决策；本实现只消费退出码，不复刻打印面。

## 依据
src/app/execute/diagnose.rs::run（退出码映射，stderr 只作消息附录）；src/platform/linux/tools.rs::check_arguments。

## 撤回条件
出现依赖 check 逐项诊断文本的外部契约，或本实现改自研检查器时重审。
