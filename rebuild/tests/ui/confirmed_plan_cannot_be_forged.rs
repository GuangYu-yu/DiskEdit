// I2：已确认计划只能由 confirm 产出，字段私有；直接拼装无法通过编译。
use diskedit::app::decide::ConfirmedPlan;
use diskedit_core::fingerprint::Fingerprint;

fn main() {
    let _plan = ConfirmedPlan {
        steps: Vec::new(),
        digest: [0; 32],
        identity: Fingerprint::strong([0; 32]),
    };
}

// covers: I2
