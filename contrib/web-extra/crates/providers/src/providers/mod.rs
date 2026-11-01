#[cfg(feature = "qwen")]
pub mod qwen;

use agent2api_provider_sdk::BoxProvider;

pub fn registry() -> Vec<BoxProvider> {
    let mut list: Vec<BoxProvider> = Vec::new();
    #[cfg(feature = "qwen")]
    list.push(Box::new(qwen::QwenProvider));
    list
}
