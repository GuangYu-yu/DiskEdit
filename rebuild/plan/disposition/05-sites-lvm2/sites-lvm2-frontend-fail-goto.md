# 批次：sites-lvm2-frontend-fail-goto（36 行）

## 判断
其余子命令内部失败清理：裁剪。

## 四态
认领 0 / 拒绝 0 / 差异 0 / 裁剪 33

## 依据
- SA2-017（已认领）：--grow-lv 次序 写表→pvresize→lvextend -r，真机锚 tests/qemu_suite.rs::s23；
- 消费面：src/app/lvm.rs::grow_chain/select_lv/pv_size_report、src/app/decide/resize.rs::pv_chain_step（refusal-map R0385-R0387、R0679-R0681）；
- 范围依据：档案表只登记 grow-lv 链，其余 lvm 子命令与库内部错误通道不实现。

## 撤回条件
- 裁剪撤回：档案表把对应子命令/库面纳入登记（如新增 PV 建立面、vgreduce/vgextend 或报告面消费）时，重新逐行判断；
- 拒绝撤回：出现依赖 lvm 内部断言语义的外部契约，或判定层等价拒绝出现缺口（R0385-R0387/R0679-R0681 失效）时重新评估；
- 认领撤回：SA2-017 的 --grow-lv 次序（写表→pvresize→lvextend -r）或判定层核验点变更时复核对应行。

## 换版重放补充（上游 2.03.16 → 2.03.44）

- 本批换版重放新增/改写待判行 6 行，全部翻态：认领 0 / 拒绝 6 / 差异 0 / 裁剪 0。
- 换版重放新 fail-goto 行（vgchange 设备导入、toollib get_vdo_settings）判拒绝（架构差异-多虑）——清理跳转由 Rust Result/所有权结构承担，非零退出经 R0386 映射；与旧迁移行（裁剪）口径不同，旧行不回改。
- 撤回条件（本节新增行）：
- 裁剪撤回：档案表把对应子命令/特性面纳入登记（如 PV 建立面、vgreduce/vgextend、报告面消费或新特性消费）时，重新逐行判断；
- 拒绝撤回：出现依赖 lvm 内部断言/失败清理语义的外部契约，或判定层等价出现缺口（R0385-R0387/R0679-R0681 失效）时重新评估；
- 认领撤回：SA2-017 的 --grow-lv 次序（写表→pvresize→lvextend -r）或判定层核验点变更时复核对应行。
