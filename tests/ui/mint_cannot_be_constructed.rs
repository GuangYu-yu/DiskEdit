// I4：铸造权封在 `transaction` 模块内；crate 外既拿不到 `Mint`，也调不动私有构造函数。
use diskedit::app::execute::transaction::Mint;

fn main() {
    let _mint = Mint::new();
}

// covers: I4
