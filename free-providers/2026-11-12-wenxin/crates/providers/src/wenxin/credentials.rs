//! 文心（千帆）凭证：两种模式、一个解析入口、一条旧版 token 续期链。
//!
//! ── 为什么一个 provider 承载两种鉴权 ─────────────────────────
//! 上游是同一家（百度千帆）的两代 API：
//!   · **API Key 模式**（现行，`qianfan.baidubce.com/v2`）：OpenAI 兼容协议，
//!     `Authorization: Bearer <API Key>`。官方鉴权文档明确「该值永久有效」
//!     （https://cloud.baidu.com/doc/qianfan-api/s/ym9chdsy5 ，2026-11-11 更新
//!     快照），**没有**续期接口 —— 这不是「还没做」，是上游没有。
//!   · **AK/SK 模式**（旧版，`aip.baidubce.com` wenxinworkshop）：AK/SK 先换
//!     `access_token`（有效期 30 天，`expires_in=2592000`），再用
//!     `?access_token=` 调对话。token 是派生凭证、可以程序化重取 ——
//!     本插件的「鉴权刷新」指这条路（ensure 临期刷新 + 401 后强制刷新，
//!     结果比较-再写回账号记录）。
//! 判据分叉在「secret 里有没有 Secret Key」：有 → AK/SK；没有 → API Key。
//!
//! ── 凭证去哪 ────────────────────────────────────────────────
//! 一律账号记录 `secret`（网关侧落库），定义文件 / pack.json / 日志永不出现。
//! 公开形态只给 `tokenTail`（尾 4 字符）与「有没有令牌」，与账号层安全边界
//! （provider-adaptation skill §2.2）同一条红线。

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::ctx::{PluginCtx, StoreAccount};
use md5::{Digest, Md5};
use serde_json::{json, Value};

use crate::http;

/// API Key 模式（现行 V2 OpenAI 兼容端点）的默认根。
pub const V2_API_BASE: &str = "https://qianfan.baidubce.com/v2";
/// AK/SK 模式的默认根（token 与对话都挂在这台主机下）。
pub const V1_ROOT: &str = "https://aip.baidubce.com";
pub const V1_OAUTH_PATH: &str = "/oauth/2.0/token";
pub const V1_CHAT_PATH: &str = "/rpc/2.0/ai_custom/v1/wenxinworkshop/chat";

pub const MODE_API_KEY: &str = "api_key";
pub const MODE_AK_SK: &str = "ak_sk";

/// 旧版 access_token 的**临期刷新窗口**：剩余寿命低于 3 天即算临期。
/// 官方有效期 30 天，3 天窗口 ≈ 剩余 10%，后台维护任务在这个窗口内换新，
/// 不等它真的过期（过期后每个请求都要先撞一次 110 才能恢复）。
pub const TOKEN_EXPIRING_WINDOW_MS: i64 = 3 * 24 * 3600 * 1000;
/// 临期判定之外，**本请求可用**的硬下限：剩余不足 5 分钟就必须现在换。
const TOKEN_HARD_FLOOR_MS: i64 = 5 * 60 * 1000;

/// 解析出的一条凭证。
#[derive(Clone, Debug)]
pub struct Credential {
    /// `api_key` | `ak_sk`。
    pub mode: &'static str,
    /// V2 API Key，或旧版的 AK。
    pub api_key: String,
    /// 仅 AK/SK 模式有值。
    pub secret_key: String,
    /// 对话根（V2：`…/v2`；V1：`{root}/rpc/2.0/ai_custom/v1/wenxinworkshop/chat`）。
    pub chat_base: String,
    /// token 端点（仅 V1 有意义）。
    pub oauth_url: String,
}

impl Credential {
    pub fn is_ak_sk(&self) -> bool {
        self.mode == MODE_AK_SK
    }
}

/// 从取值链挑第一个非空字符串（记录可能被手工编辑过，宽容读取）。
fn pick(record: &Value, keys: &[&str]) -> String {
    for key in keys {
        let value = record
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !value.is_empty() {
            return value.to_string();
        }
    }
    String::new()
}

/// 尾 4 字符（公开形态用）。按 `char` 取，避免误粘贴的多字节字符被腰斩。
pub fn token_tail(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    let start = chars.len().saturating_sub(4);
    chars[start..].iter().collect()
}

/// 去尾斜杠的 baseUrl 规整（用户粘贴 `…/v2/` 很常见）。
fn normalize_base(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_string()
}

/// 解析账号 `secret`。唯一入口；报错文案要能指引用户怎么修。
pub fn parse(secret: &Value) -> Result<Credential, String> {
    let base_override = pick(secret, &["baseUrl", "base_url", "endpoint"]);
    let explicit_mode = pick(secret, &["mode", "authMode"]).to_lowercase();
    let api_key = pick(secret, &["apiKey", "api_key", "token", "key"]);
    let secret_key = pick(secret, &["secretKey", "secret_key", "sk", "secret"]);
    if api_key.is_empty() {
        return Err(
            "文心账号缺少凭证：请提供千帆 API Key（bce-v3/… 开头），或 AK/SK 两个键".to_string(),
        );
    }
    if api_key.len() > 4096 || secret_key.len() > 4096 || api_key.contains(['\r', '\n']) {
        return Err("文心凭证格式不对（过长或含换行），请重新粘贴".to_string());
    }
    // 显式 mode 优先；否则「有 Secret Key = AK/SK」。
    let is_ak_sk = match explicit_mode.as_str() {
        MODE_AK_SK => true,
        MODE_API_KEY => false,
        other if !other.is_empty() => {
            return Err(format!("文心账号的 mode 不认识：{other}（可用 api_key / ak_sk）"))
        }
        _ => !secret_key.is_empty(),
    };
    if is_ak_sk && secret_key.is_empty() {
        return Err("文心账号声明了 AK/SK 模式但缺少 Secret Key，请补齐或改用 API Key".to_string());
    }
    let (mode, chat_base, oauth_url) = if is_ak_sk {
        let root = if base_override.is_empty() {
            V1_ROOT.to_string()
        } else {
            normalize_base(&base_override)
        };
        (MODE_AK_SK, format!("{root}{V1_CHAT_PATH}"), format!("{root}{V1_OAUTH_PATH}"))
    } else {
        let base = if base_override.is_empty() {
            V2_API_BASE.to_string()
        } else {
            normalize_base(&base_override)
        };
        (MODE_API_KEY, base, String::new())
    };
    Ok(Credential { mode, api_key, secret_key, chat_base, oauth_url })
}

/// 账号稳定身份（去重用）：模式 + 凭证指纹。SK 参与指纹但只出哈希。
pub fn identity_of(cred: &Credential) -> String {
    let mut hasher = Md5::new();
    hasher.update(cred.mode.as_bytes());
    hasher.update(b"|");
    hasher.update(cred.api_key.as_bytes());
    if !cred.secret_key.is_empty() {
        hasher.update(b"|");
        hasher.update(cred.secret_key.as_bytes());
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("wenxin-{}", &hex[..12.min(hex.len())])
}

/// 读旧版缓存的 access_token（**没过期**才返回）。
pub fn cached_v1_token(secret: &Value) -> Option<(String, i64)> {
    let token = pick(secret, &["accessToken", "access_token"]);
    let expires_at = secret
        .get("expiresAt")
        .and_then(Value::as_i64)
        .or_else(|| secret.get("expires_at").and_then(Value::as_i64))
        .unwrap_or(0);
    if token.is_empty() || expires_at <= http::now_ms() + TOKEN_HARD_FLOOR_MS {
        return None;
    }
    Some((token, expires_at))
}

/// 临期判定（维护任务的入场券）。API Key 模式恒 false（永久有效，见模块头）。
pub fn token_expiring(secret: &Value) -> bool {
    let expires_at = secret
        .get("expiresAt")
        .and_then(Value::as_i64)
        .or_else(|| secret.get("expires_at").and_then(Value::as_i64))
        .unwrap_or(0);
    expires_at > 0 && expires_at - http::now_ms() < TOKEN_EXPIRING_WINDOW_MS
}

/// 换一枚新 access_token（打 oauth 端点）。
///
/// 返回 `(token, expires_at_ms)`。oauth 错误形态是
/// `{"error": "...", "error_description": "..."}`（与业务错误码不同族），
/// 原样带出，让调用方归到可读的失败文案里。
pub async fn fetch_v1_token(
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
    cred: &Credential,
) -> Result<(String, i64), String> {
    let client = http::client(proxy, gateway_proxy);
    let url = format!(
        "{}?grant_type=client_credentials&client_id={}&client_secret={}",
        cred.oauth_url,
        urlencode(&cred.api_key),
        urlencode(&cred.secret_key),
    );
    let response = client
        .post(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|error| format!("换取 access_token 失败：{error}"))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|error| format!("读取 access_token 响应失败：{error}"))?;
    let payload: Value = serde_json::from_str(&text)
        .map_err(|_| format!("access_token 响应不是 JSON（HTTP {status}）"))?;
    if let Some(token) = payload.get("access_token").and_then(Value::as_str) {
        if !token.is_empty() {
            let expires_in = payload
                .get("expires_in")
                .and_then(Value::as_i64)
                .unwrap_or(30 * 24 * 3600);
            // 打 95 折：官方给的 expiry 按秒取整，别贴着边界用。
            let expires_at = http::now_ms() + (expires_in * 1000 * 95) / 100;
            return Ok((token.to_string(), expires_at));
        }
    }
    let error = pick(&payload, &["error", "error_code"]);
    let description = pick(&payload, &["error_description", "error_msg"]);
    Err(format!(
        "换取 access_token 被拒绝（HTTP {status}）：{} {}",
        if error.is_empty() { "未知错误" } else { &error },
        description
    ))
}

/// 把新 token 合并回 secret（其余键原样保留，供比较-再写）。
pub fn merge_v1_token(secret: &Value, token: &str, expires_at: i64) -> Value {
    let mut merged = secret.clone();
    if let Some(object) = merged.as_object_mut() {
        object.insert("accessToken".to_string(), json!(token));
        object.insert("expiresAt".to_string(), json!(expires_at));
    }
    merged
}

/// 比较再写：把刷新结果落回账号记录。返回 false = 期间被别人改过
/// （并发刷新互斥由网关侧的 expect 机制保证，这里只如实上报）。
pub async fn save_refreshed_token(
    ctx: &PluginCtx,
    record: &StoreAccount,
    token: &str,
    expires_at: i64,
) -> Result<bool, String> {
    let merged = merge_v1_token(&record.secret, token, expires_at);
    ctx.store_update(&record.id, &record.secret, &merged)
        .await
        .map_err(|error| format!("写回刷新后的 access_token 失败：{error}"))
}

/// 查询串转义（AK/SK 进 URL，必须编码；`%` 编不了的字节按 UTF-8 兜底）。
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// API Key 模式的**轻量探活**：`GET {base}/models`。
///
/// ── 只把「上游明确拒绝」当成失败 ────────────────────────────
/// 401 / 403 是「这把 Key 不被接受」的确定结论（官方错误码表：
/// `invalid_iam_token` / `invalid_model` / `invalid_appid`），此时拒绝落库
/// 是对的。网络层失败（离线、代理没配好、DNS）**不拦** —— 否则用户连
/// 「先把账号加进来、回头再修网络」都做不到，而那种失败与 Key 本身无关。
pub async fn probe_api_key(
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
    cred: &Credential,
) -> Result<(), String> {
    if cred.is_ak_sk() {
        return Ok(());
    }
    let client = http::client(proxy, gateway_proxy);
    let response = match client
        .get(format!("{}/models", cred.chat_base))
        .header("Authorization", format!("Bearer {}", cred.api_key))
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => return Ok(()),
    };
    let status = response.status().as_u16();
    if status == 401 || status == 403 {
        let text = response.text().await.unwrap_or_default();
        let code = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|payload| {
                payload
                    .pointer("/error/code")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_default();
        return Err(format!(
            "千帆 API Key 被上游拒绝（HTTP {status}{}）：请确认 Key 有效、且已授权千帆 \
             V2 推理资源（控制台「安全认证 API Key」→ 配置资源）",
            if code.is_empty() { String::new() } else { format!("，{code}") }
        ));
    }
    Ok(())
}
