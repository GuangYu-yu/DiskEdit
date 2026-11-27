# sites-e2fsprogs-check-error-report

## 判断
e2fsck 全部 com_err/printf 文案站点（486 行）。全批 486 行已逐行翻态，note 见同名 tsv。

## 依据
SA2-001/002；D3/D9；src/app/execute/diagnose.rs::run

## 撤回条件
出现依赖 e2fsck 文案措辞的外部契约（§10 登记），或本实现改为自研 e2fsck 时重审。
