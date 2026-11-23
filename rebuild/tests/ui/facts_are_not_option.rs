// 事实位不得用 `Option` 表达"不知道"：缺席与未观测是两件事。
use diskedit_core::evidence::Observation;

fn main() {
    let _fact: Observation<u64> = None;
}
