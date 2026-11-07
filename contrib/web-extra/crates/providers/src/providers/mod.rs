#[cfg(feature = "qwen")]
pub mod qwen;

use agent2api_provider_sdk::BoxProvider;

/// 本包声明的 provider 注册表；`pack.json` 的 `providers[]` 必须与这里一一对应。
pub fn registry() -> Vec<BoxProvider> {
    let mut list: Vec<BoxProvider> = Vec::new();
    #[cfg(feature = "qwen")]
    list.push(Box::new(qwen::QwenProvider));
    list
}
