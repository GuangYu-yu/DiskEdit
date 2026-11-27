# sites-e2fsprogs-check-fatal-exit

## 判断
e2fsck 出口站点：7 行退出码产出端认领（01845/01847/01913/01914/01915/01960/01965），47 行 usage/辅助程序裁剪。全批 54 行已逐行翻态，note 见同名 tsv。

## 依据
SA2-001；src/app/execute/filesystem.rs::exit_is_clean；src/app/execute/diagnose.rs::run；tests/online.rs::a_repaired_filesystem_is_a_success_and_a_later_failure_is_partial

## 撤回条件
认领行：若 e2fsck 退出码语义变化（版本更新）或本实现改自研检查器，重审映射 0/1/2=成功、其余=失败；裁剪行：出现依赖 usage 码/辅助程序的外部契约时重审。
