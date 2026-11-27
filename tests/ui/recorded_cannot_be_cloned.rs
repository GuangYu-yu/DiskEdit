// I4：写令牌不可复制，跨步复用与重放无法通过编译。
use diskedit::app::execute::Recorded;

fn requires_clone<T: Clone>() {}

fn main() {
    requires_clone::<Recorded>();
}

// covers: I4
