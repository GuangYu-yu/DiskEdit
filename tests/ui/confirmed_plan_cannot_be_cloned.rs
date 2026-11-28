// 已确认计划不可复制，一份确认不会派生两份可执行副本。
use diskedit::app::decide::ConfirmedPlan;

fn requires_clone<T: Clone>() {}

fn main() {
    requires_clone::<ConfirmedPlan>();
}
