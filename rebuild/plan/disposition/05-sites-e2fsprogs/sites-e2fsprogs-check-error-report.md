# sites-e2fsprogs-check-error-report

## 判断
e2fsck 全部 com_err/printf 文案站点（491 行）。全批 486 行已逐行翻态，note 见同名 tsv。

## 依据
SA2-001/002；D3/D9；src/app/execute/diagnose.rs::run

## 撤回条件
出现依赖 e2fsck 文案措辞的外部契约（§10 登记），或本实现改为自研 e2fsck 时重审。

## 换版重放补充（2026-11-27：e2fsprogs 1.47.0→1.47.4）

- 本批新增/改写 9 行（00927/00960/00963/01004/01016/01020/01239/01859/01860，1.47.4 新增 ea_inode 族、zero dtime、超前 mount 时间的 fix_problem 站点），全部判**裁剪**：修复判读消息属 e2fsck 修复器本体，本实现只消费退出码（SA2-001/002）。

**依据**：plan/baseline/incident-classes.tsv（行内引 SA id）；plan/baseline/refusal-map.tsv；行原文 plan/baseline/reference/sites/*.tsv；上游源码对照 /tmp/refsrc（新）与 /tmp/src（旧）。

**撤回条件**：本实现引入任何 fs 修复能力、改为自研 e2fsck，或出现依赖 e2fsck 文案措辞的外部契约（须先按手册 §10 登记）时重审。
