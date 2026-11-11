#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod http;
mod wenxin;

use agent2api_provider_sdk::BoxProvider;

fn main() {
    agent2api_provider_sdk::serve_blocking(providers::registry());
}

mod providers {
    use super::BoxProvider;

    pub fn registry() -> Vec<BoxProvider> {
        vec![Box::new(super::wenxin::WenxinProvider)]
    }
}
