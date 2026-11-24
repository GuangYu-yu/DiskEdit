// I1：执行结果只能由状态机推进产出，字段私有；直接拼装无法通过编译。
use diskedit::app::execute::{ExecutionState, Outcome};

fn main() {
    let _outcome = Outcome {
        state: ExecutionState::Committed,
        failure: None,
        reason: None,
    };
}
