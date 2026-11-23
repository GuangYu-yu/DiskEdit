// 事实位是 `Observation<T>`，不是内层值：未观测的事实不能冒充已观测的值。
use diskedit_core::evidence::Observation;

fn main() {
    let fact: Observation<u64> = Observation::unknown("探测结果不可解析");
    let _value: u64 = fact;
}
