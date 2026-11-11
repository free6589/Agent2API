//! Free Pi 的转发（OpenAI 兼容直通）与错误分类（四档）。
//!
//! ── build_chat_request（`free-pi-cli/src/provider.ts` 的网关侧对应物）──
//! 上游就是标准 OpenAI Chat Completions：`POST {BASE_URL}/v1/chat/completions`，
//! JWT 走标准 Bearer，body 除 `model` 非空校验外**逐字节透传**（模型 id 就是
//! 目录里的 id，服务端自己解析；没有别名映射、没有协议翻译）。带上的两个
//! 伪装头都有上游闸门在消费：
//!   · `x-session-id`：R19 并发会话闸门的判定键（同一 JWT 的第二个活跃会话
//!     回 409 `concurrent_session`）。取值 = **网关入站请求里的会话 id**
//!     （客户端带来什么透什么，归一化与 `plugin_host::rpc::session_of`
//!     逐字一致：trim；空 / 超 256 字符按「没有会话 id」处理，此时不发这个头
//!     —— 头在，值编不出来就是不发，绝不本地造一个假 id）；
//!   · `x-client-version`：强制更新闸门（低于 `minCliVersion` 回 426
//!     `upgrade_required`）。取 CLI 当前版本常量（`super::CLIENT_VERSION`）。
//!
//! ── classify_error（判据全部来自上游自己的错误契约，零猜测）────────
//! 上游的错误信封（`free-pi-cli/src/error-notice.ts::parseErrorEnvelope` +
//! `vendor/shared/errors.ts`）：
//!
//! ```json
//! { "code": "daily_cap", "message": "…",
//!   "limit": "daily_cap", "retry_after_seconds": 720,
//!   "clears_at": "2026-11-13T16:00:00.000Z",
//!   "remaining_credits_today": 0, "allowance_credits": 4200,
//!   "lane": "slow", "hourly_ceiling": 30 }
//! ```
//!
//! code → HTTP 状态的**实测映射**（`vendor/shared/errors.ts::ERROR_HTTP_STATUS`）：
//! 429 = `daily_cap` / `queue_wait` / `rolling_cap` / `concurrent` /
//! `hourly_ceiling`；409 = `concurrent_session`；413 = `context_ceiling`；
//! 502 = `upstream_error`；503 = `global_cap`；403 = `consent_required` /
//! `account_review` / `ads_required`；426 = `upgrade_required`。429 文案由
//! `vendor/shared/credits.ts::formatLimitNotice` 生成，额度类是
//! "daily limit, …"，速率窗口类是 "hourly limit, back in N min, …" /
//! "rolling limit, …" / "one request at a time" / "queue wait"。
//!
//! 四档归档（provider-adaptation SKILL §2.6；动作在编排层）：
//!   · **401 → `TokenExpired`**：JWT 被上游拒收。本家无续期
//!     （`supports_refresh = false`，且 `refresh_access_token` 显式返回 Err，
//!     见 `super` 里那段覆写说明 —— 否则编排层会拿同一枚死 JWT 再打一次），
//!     编排层因此直接落动作 2b：给账号标 `needsRelogin` 并把错误原样透传
//!     —— 正是「90 天重贴」流；
//!   · **429 → `QuotaLimited`**，额度 / 速率按**观察到的字段**区分
//!     （两者都用 429 表达、处置完全不同：额度冷却到恢复时刻，速率窗口是
//!     分钟级）：
//!       - 额度类 = `code`/`limit` ∈ {daily_cap}，或文案含 "daily limit"
//!         （daily_cap 的 label，`credits.ts::LIMIT_NOTICE_LABELS`）；
//!       - 速率窗口类 = `code`/`limit` ∈ {hourly_ceiling, rolling_cap,
//!         concurrent, queue_wait}，或文案含 "hourly limit" / "rolling limit" /
//!         "one request at a time" / "queue wait"；
//!       - `reset_at`：`clears_at`（ISO 8601）优先，退 `retry_after_seconds`
//!         （现在 + 秒），都解析不出 None（编排层给兜底冷却）；
//!       - `global_cap`（503 "service daily limit"）是**服务级**日额度，
//!         冷却 / 换账号都无济于事（所有账号共享同一个池），归 Fatal 透传。
//!   · **内容策略拦截 → `ContentBlocked`**：上游错误契约里没有内容审核类
//!     code；其余 4xx 交给本仓共用的 `content_block::classify_or_fatal`
//!     （命中共用规则才拦，不命中按 Fatal 透传）；
//!   · **其余 → `Fatal`**：409（`concurrent_session`，CLI 明说这不是限流、
//!     客户端不得自动重试）、413（上下文超限）、426（CLI 过旧，重试无益）、
//!     5xx（`upstream_error` / `global_cap`，见上）、裸 4xx。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic。

use axum::http::HeaderMap;
use serde_json::Value;

use super::super::adapter::{ChatRequestPlan, UpstreamErrorClass};
use super::{BASE_URL, CHAT_PATH, CLIENT_VERSION};
use crate::server::errors::GatewayError;

/// 入站会话 id 的归一化（与 `plugin_host::rpc::session_of` 逐字一致：
/// trim；空 / 超 256 字符按「没有会话 id」—— 超长截断会让两个不同会话在
/// 前 256 位相同时碰撞，比「没有会话」更坏）。
pub fn session_id_from_headers(headers: &HeaderMap) -> String {
    let value = headers
        .get("x-session-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .trim();
    if value.is_empty() || value.chars().count() > 256 {
        return String::new();
    }
    value.to_string()
}

/// 准备一次上游调用。
pub fn build_chat_request(
    account: &Value,
    body: &Value,
    client_headers: &HeaderMap,
) -> Result<ChatRequestPlan, GatewayError> {
    // 令牌读取口径与本仓内置家一致（会话形态的 `auth.accessToken`，
    // `account_store::store::session_from_record` 的产物）。
    let access_token = account
        .get("auth")
        .and_then(|auth| auth.get("accessToken"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if access_token.is_empty() {
        return Err(GatewayError::with_status(
            401,
            "Free Pi 账号缺少登录令牌，请重新粘贴 JWT",
        ));
    }

    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if requested.is_empty() {
        return Err(GatewayError::with_status(400, "请求缺少 model 字段"));
    }

    let mut payload = body.clone();
    if let Some(object) = payload.as_object_mut() {
        // 模型 id 原样透传：目录 id 就是请求 model（服务端按 model_catalog
        // 解析，见 `free-pi-cli/src/provider.ts` #140 注释），无别名映射。
        object.insert("model".to_string(), Value::String(requested));
    }

    let mut headers = vec![
        ("authorization".to_string(), format!("Bearer {access_token}")),
        ("content-type".to_string(), "application/json".to_string()),
        ("accept".to_string(), "application/json".to_string()),
        // 强制更新闸门读它（低于 minCliVersion 回 426）
        ("x-client-version".to_string(), CLIENT_VERSION.to_string()),
    ];
    // 并发会话闸门读它（同一 JWT 的第二个活跃会话回 409）。网关带来了
    // 会话 id 就透传；没有就不发 —— 假 id 会把两个真实会话并成一个租约。
    let session_id = session_id_from_headers(client_headers);
    if !session_id.is_empty() {
        headers.push(("x-session-id".to_string(), session_id));
    }

    Ok(ChatRequestPlan::chat(
        format!("{BASE_URL}{CHAT_PATH}"),
        headers,
        payload,
    ))
}

/// 错误信封里的一个字符串字段（顶层，或 `error` 嵌套对象里）。
fn envelope_text(body: &Value, keys: &[&str]) -> Option<String> {
    let read = |value: &Value| -> Option<String> {
        keys.iter()
            .find_map(|key| value.get(*key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    read(body).or_else(|| body.get("error").and_then(read))
}

/// 错误信封里的 `code` / `limit`（`limit` 与 code 同值是上游的约定，
/// `error-notice.ts` 的解析顺序同源）。
fn envelope_code(body: &Value) -> Option<String> {
    envelope_text(body, &["code"]).or_else(|| envelope_text(body, &["limit"]))
}

/// 错误信封里的数字字段（`retry_after_seconds`）。
fn envelope_number(body: &Value, key: &str) -> Option<f64> {
    body.get(key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value >= 0.0)
}

/// 上游原文摘要（信封 message 优先，退整个 body 截断 —— 给客户端可见文案
/// 与日志用，绝不包含凭证）。
fn upstream_excerpt(status: u16, body: &Value, message: Option<String>) -> String {
    let detail = message.unwrap_or_else(|| {
        let raw = body.to_string();
        raw.chars().take(200).collect()
    });
    format!("上游返回 {status}: {detail}")
}

/// `clears_at`（ISO 8601）→ 毫秒时刻；解析不出返回 None（不猜）。
fn clears_at_to_ms(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|time| time.timestamp_millis())
        .filter(|ms| *ms > 0)
}

/// 429 的额度 / 速率区分（判据来源见模块头）。
///
/// 结构化 `code`/`limit` 字段优先（服务端契约字段），文案关键词兜底
/// （信封 message 就是 `formatLimitNotice` 生成的那行，label 词表固定）。
fn is_daily_quota(code: Option<&str>, message: &str) -> bool {
    if let Some(code) = code {
        return code == "daily_cap";
    }
    message.contains("daily limit")
}

fn is_rate_window(code: Option<&str>, message: &str) -> bool {
    if let Some(code) = code {
        return matches!(
            code,
            "hourly_ceiling" | "rolling_cap" | "concurrent" | "queue_wait"
        );
    }
    ["hourly limit", "rolling limit", "one request at a time", "queue wait"]
        .iter()
        .any(|marker| message.contains(marker))
}

/// 恢复时刻：`clears_at` 优先，退 `retry_after_seconds`（现在 + 秒）。
fn reset_at_of(body: &Value) -> Option<i64> {
    if let Some(clears_at) = envelope_text(body, &["clears_at"]) {
        if let Some(ms) = clears_at_to_ms(&clears_at) {
            return Some(ms);
        }
    }
    let retry_after = envelope_number(body, "retry_after_seconds")?;
    let seconds = retry_after.max(0.0) as i64;
    Some(crate::server::logging::now_ms() + seconds.saturating_mul(1000))
}

/// 错误分类（四档；判据来源见模块头）。
pub fn classify_error(status: u16, error_body: &Value) -> UpstreamErrorClass {
    let message = envelope_text(error_body, &["message"]);
    let code = envelope_code(error_body);
    let excerpt = || upstream_excerpt(status, error_body, message.clone());

    match status {
        // JWT 被上游拒收 → 走「无续期家的 401」既有链路（编排层落
        // needsRelogin，客户端看到原文；用户重贴一枚新 JWT 即恢复）。
        401 => UpstreamErrorClass::TokenExpired { message: excerpt() },
        429 => {
            let text = message.clone().unwrap_or_default();
            let qualifier = if is_daily_quota(code.as_deref(), &text) {
                "按额度耗尽处理：冷却到上游给出的恢复时刻"
            } else if is_rate_window(code.as_deref(), &text) {
                "按速率窗口处理：短冷却后重试"
            } else {
                // 两种判据都不命中（形状陌生的 429）按额度处理：宁可冷却，
                // 也不把限额当 Fatal —— 后者会让账号永不轮换，每次都撞同一面墙。
                "按额度处理（上游未给出可识别的额度类型）"
            };
            UpstreamErrorClass::QuotaLimited {
                reset_at: reset_at_of(error_body),
                message: format!("{}（{qualifier}）", excerpt()),
                upstream_code: None,
                status,
            }
        }
        // 其余状态码先给共用的内容策略分类一次机会（它命中才拦，否则 Fatal）
        _ => crate::server::core::providers::content_block::classify_or_fatal(
            status,
            error_body,
            excerpt(),
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn headers_with_session(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", value.parse().expect("合法头值"));
        headers
    }

    /// 请求计划：URL、Bearer、伪装三头、模型 id 透传。
    #[test]
    fn build_chat_request_targets_freepi_v1_with_auth_and_camouflage() {
        let account = json!({ "auth": { "accessToken": "jwt-token" } });
        let body = json!({
            "model": "deepseek/deepseek-v4-flash",
            "messages": [{ "role": "user", "content": "hi" }],
            "stream": true
        });
        let plan = build_chat_request(&account, &body, &headers_with_session("sess-1"))
            .expect("计划应当构造成功");
        assert_eq!(
            plan.url,
            "https://api.freepi.ai/v1/chat/completions",
            "上游 base 是 https://api.freepi.ai/v1（chat/completions 路由）"
        );
        let header = |name: &str| {
            plan.headers
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(header("authorization").as_deref(), Some("Bearer jwt-token"));
        assert_eq!(header("x-session-id").as_deref(), Some("sess-1"));
        assert_eq!(header("x-client-version").as_deref(), Some(super::super::CLIENT_VERSION));
        assert_eq!(
            plan.body.get("model").and_then(Value::as_str),
            Some("deepseek/deepseek-v4-flash"),
            "模型 id 必须原样透传"
        );
        assert_eq!(plan.response, super::super::super::adapter::UpstreamResponse::Chat);
    }

    /// 会话 id 归一化：客户端带了就透；没带 / 超长不发这个头（不编造假 id）。
    #[test]
    fn session_header_follows_gateway_session_id() {
        let account = json!({ "auth": { "accessToken": "jwt" } });
        let body = json!({ "model": "deepseek/deepseek-v4-flash", "messages": [] });

        let without = build_chat_request(&account, &body, &HeaderMap::new()).unwrap();
        assert!(
            !without.headers.iter().any(|(key, _)| key == "x-session-id"),
            "入站没有会话 id 时不发 x-session-id"
        );

        let long = "s".repeat(257);
        let oversized = build_chat_request(&account, &body, &headers_with_session(&long)).unwrap();
        assert!(
            !oversized.headers.iter().any(|(key, _)| key == "x-session-id"),
            "超长会话 id 按「没有会话」处理（截断会碰撞）"
        );

        let with = build_chat_request(&account, &body, &headers_with_session("  abc  ")).unwrap();
        assert_eq!(
            with.headers
                .iter()
                .find(|(key, _)| key == "x-session-id")
                .map(|(_, value)| value.as_str()),
            Some("abc"),
            "两端空白剥掉后透传"
        );
    }

    /// 缺令牌 / 缺 model 的明确报错。
    #[test]
    fn build_chat_request_rejects_missing_token_or_model() {
        let body = json!({ "model": "m", "messages": [] });
        let error = build_chat_request(&json!({}), &body, &HeaderMap::new())
            .err()
            .expect("缺令牌应当被拒");
        assert_eq!(error.status_code, 401);

        let account = json!({ "auth": { "accessToken": "jwt" } });
        let error = build_chat_request(&account, &json!({ "messages": [] }), &HeaderMap::new())
            .err()
            .expect("缺 model 应当被拒");
        assert_eq!(error.status_code, 400);
    }

    /// 401 → TokenExpired；5xx → Fatal；裸 4xx 走共用内容分类后落 Fatal。
    #[test]
    fn classify_maps_401_and_5xx() {
        // 401 的正文形状按上游信封契约取 `{message}`（CLI 对 401 只判状态码，
        // `auth-flow.ts`：`if (res.status === 401) throw new UnauthorizedError()`），
        // 不编造未观测过的 code。
        let body = json!({ "message": "unauthorized" });
        match classify_error(401, &body) {
            UpstreamErrorClass::TokenExpired { message } => {
                assert!(message.contains("上游返回 401"), "文案带状态与原文：{message}");
            }
            other => panic!("401 应当是 TokenExpired，实际 {other:?}"),
        }
        let upstream_error = json!({ "code": "upstream_error", "message": "boom" });
        assert!(matches!(
            classify_error(502, &upstream_error),
            UpstreamErrorClass::Fatal { .. }
        ));
        assert!(matches!(
            classify_error(500, &json!({ "message": "internal" })),
            UpstreamErrorClass::Fatal { .. }
        ));
    }

    /// 429 额度 / 速率区分：结构化 code 优先，文案兜底；恢复时刻按
    /// clears_at / retry_after_seconds 解析。
    #[test]
    fn classify_429_distinguishes_quota_and_rate_by_observed_fields() {
        // 额度耗尽：daily_cap（clears_at 给恢复时刻）
        let daily = json!({
            "code": "daily_cap", "limit": "daily_cap",
            "message": "daily limit, 0 credits left today",
            "clears_at": "2026-11-13T16:00:00.000Z",
            "remaining_credits_today": 0
        });
        match classify_error(429, &daily) {
            UpstreamErrorClass::QuotaLimited { reset_at, message, .. } => {
                assert_eq!(
                    reset_at,
                    Some(
                        chrono::DateTime::parse_from_rfc3339("2026-11-13T16:00:00.000Z")
                            .unwrap()
                            .timestamp_millis()
                    ),
                    "clears_at 应当解析成恢复时刻"
                );
                assert!(message.contains("额度耗尽"), "额度类要有语义标注：{message}");
            }
            other => panic!("429 应当是 QuotaLimited，实际 {other:?}"),
        }

        // 速率窗口：hourly_ceiling（retry_after_seconds 分钟级）
        let hourly = json!({
            "code": "hourly_ceiling", "limit": "hourly_ceiling",
            "message": "hourly limit, back in 12 min, 4,200 credits left today",
            "retry_after_seconds": 720
        });
        match classify_error(429, &hourly) {
            UpstreamErrorClass::QuotaLimited { reset_at, message, .. } => {
                assert!(reset_at.is_some(), "retry_after_seconds 也给恢复时刻");
                assert!(message.contains("速率窗口"), "速率类要有语义标注：{message}");
            }
            other => panic!("429 应当是 QuotaLimited，实际 {other:?}"),
        }

        // 文案兜底：没有结构化字段时按 formatLimitNotice 的固定 label 区分
        let daily_text = json!({ "message": "daily limit, back at 12:00 AM, 0 credits left today" });
        assert!(matches!(
            classify_error(429, &daily_text),
            UpstreamErrorClass::QuotaLimited { .. }
        ));
        let rate_text = json!({ "message": "hourly limit, back in 5 min, 100 credits left today" });
        assert!(matches!(
            classify_error(429, &rate_text),
            UpstreamErrorClass::QuotaLimited { .. }
        ));
    }

    /// 上游明确「不是限流」的形态如实落档：409 concurrent_session 与
    /// 503 global_cap 都是 Fatal（服务级 / 会话级状态，换账号无济于事）。
    #[test]
    fn classify_maps_non_limit_states_to_fatal() {
        let concurrent = json!({
            "code": "concurrent_session",
            "message": "another session is active"
        });
        assert!(matches!(
            classify_error(409, &concurrent),
            UpstreamErrorClass::Fatal { .. }
        ));
        let global = json!({
            "code": "global_cap", "limit": "global_cap",
            "message": "service daily limit, back in 30 min"
        });
        assert!(matches!(
            classify_error(503, &global),
            UpstreamErrorClass::Fatal { .. }
        ));
        let upgrade = json!({ "code": "upgrade_required", "message": "update the CLI" });
        assert!(matches!(
            classify_error(426, &upgrade),
            UpstreamErrorClass::Fatal { .. }
        ));
    }
}
