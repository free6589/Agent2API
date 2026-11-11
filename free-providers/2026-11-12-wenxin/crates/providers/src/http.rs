//! 出网 HTTP 客户端：按「代理键」缓存的 reqwest 客户端。
//!
//! 形状抄自本仓库同代插件的公共 `http.rs`（Agent2WebProviders）：控制面
//! 20 秒超时、流式对话在请求上单独给长超时；代理为空 = 显式直连（控制面
//! 不跟随系统代理，避免「配了坏代理连控制面都打不通」）。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use agent2api_provider_protocol as proto;

pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(20);
pub const CHAT_TIMEOUT: Duration = Duration::from_secs(600);
const DEFAULT_USER_AGENT: &str = "undici";

fn cache() -> &'static Mutex<HashMap<String, reqwest::Client>> {
    static CACHE: OnceLock<Mutex<HashMap<String, reqwest::Client>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 取（或建）一个按代理键缓存的客户端；`proxy = None` 时用网关默认代理。
pub fn client(proxy: Option<&proto::WireProxy>, gateway_proxy: &str) -> reqwest::Client {
    let key = match proxy {
        Some(proxy) => proxy.to_url(),
        None => gateway_proxy.trim().to_string(),
    };
    if let Ok(guard) = cache().lock() {
        if let Some(client) = guard.get(&key) {
            return client.clone();
        }
    }
    let built = build(&key);
    if let Ok(mut guard) = cache().lock() {
        guard.insert(key, built.clone());
    }
    built
}

fn build(proxy_url: &str) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .use_rustls_tls()
        .timeout(CONTROL_TIMEOUT)
        .user_agent(DEFAULT_USER_AGENT);
    let trimmed = proxy_url.trim();
    if trimmed.is_empty() || trimmed == "direct" {
        builder = builder.no_proxy();
    } else {
        match reqwest::Proxy::all(trimmed) {
            Ok(proxy) => builder = builder.proxy(proxy),
            Err(_) => builder = builder.no_proxy(),
        }
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}
