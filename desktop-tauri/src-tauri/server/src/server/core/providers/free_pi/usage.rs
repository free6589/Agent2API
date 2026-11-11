//! Free Pi 的余额查询：`GET /me/stats`（官方 CLI 用量工具同一条接口）。
//!
//! ── 接口依据（全部来自官方 CLI 源码，本仓库 `free-pi-cli/` 检出）──
//! `free-pi-cli/src/usage-tool.ts::fetchUsageText`：`GET {BASE_URL}/me/stats`、
//! 鉴权就是账号自己的 `Authorization: Bearer <jwt>`（转发用的同一枚 90 天
//! JWT，没有第二份查询凭证）、5 秒短超时；响应形状是
//! `vendor/shared/api.ts::MeStatsResponseSchema`（tier / cap_usd_today /
//! spent_usd_today / credit_credits / allowance_credits /
//! remaining_credits_today / refill_at / lane / bonus_credits / 今日与终身
//! token 数 / lifetime）。本模块的超时放宽到 15 秒 —— 那是 CLI 在用户终端
//! 里的取值，这里是网关侧的后台查询（余额实现的同一档约定，15~20 秒）。
//!
//! ── 展示口径：percent / credits，永不用美元 ───────────────────
//! 上游自己就定了这条规矩（`usage-tool.ts` 的模块注释：founder decision
//! 2026-09-01「never show free-usage dollars」+ `#credits` 的 display peg）：
//!   · 免费额度按「今日已用 N%」或「剩 X / Y credits」展示 —— 上游响应里的
//!     `*_usd` 字段（cap/spent/remaining_usd_today）**只用来算百分比**
//!     （`percent_used` 逐条对照 CLI 的 `percentUsed`），绝不进任何展示串；
//!   · 购买余额按 `credit_credits`（服务端持有的 display peg）显示为整数
//!     credits，同样不带美元；
//!   · `bonus_credits` 的上游语义是 "purchased plus granted bonus"，与
//!     `credit_credits` 有重叠关系，加总会把同一份积分算两遍 —— 它只进
//!     `raw`（排障），不进加总与明细行。
//!
//! ── raw 脱敏 ────────────────────────────────────────────────
//! 归一化形状的 `raw` 会随 usage JSON 原样下发到前端（排障用、界面默认不
//! 展示，但「不展示」不是「不发送」）：USD 计价字段全部剥掉
//! （cap/spent/remaining_usd_today、credit_usd、lifetime.spent_usd），
//! 「展示不用美元」因此延伸到「美元字段根本不出网关」。
//!
//! ── available 的口径 ────────────────────────────────────────
//! `available` 是低余额跳过 / 禁用判定的读数（`usage_records::extract_remaining`
//! 逐形状探测）：取 `remaining_credits_today`（今日免费剩余，上游注明不含
//! 购买部分）+ `credit_credits`（购买余额）—— 两个都能读到的和才是「这个
//! 账号今天还能不能发请求」的真实答案；只读到其一就取其一，全读不到给 null
//! （选路按「无法判定」放行，与别家同口径）。
//!
//! ── 凭证红线 ────────────────────────────────────────────────
//! JWT 本体只进 Authorization 头；错误文案、日志、返回值里都不出现
//! （与 `account_store::freepi_accounts` 的公开形态红线同一条）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本文件零 unwrap/expect/panic。

use chrono::TimeZone;
use serde_json::{json, Map, Value};

use super::models;
use super::BASE_URL;
use crate::server::core::account_store::AccountStore;
use crate::server::core::auth_http::send_raw;
use crate::server::core::proxies::{session_proxy, ResolvedProxy};
use crate::server::core::providers::adapter::USAGE_NOT_CONFIGURED_CODE;
use crate::server::errors::GatewayError;

/// 余额请求超时（与内置家余额实现同一档：15~20 秒；CLI 自己用 5 秒，
/// 那是用户终端里的取值，见模块头）。
const REQUEST_TIMEOUT_MS: u64 = 15_000;

/// 余额接口路径（上游根下一级，**不带 `/v1`** —— `usage-tool.ts` 的
/// `fetchUsageText` 就这么调，与目录接口同一条根路径约定）。
pub(super) const ME_STATS_PATH: &str = "/me/stats";

/// 查询某账号的余额 / 用量（`ProviderAdapter::query_usage` 的本家实现）。
///
/// 失败语义与内置家一致：`Err` 由调用方写进结果的 `error` 字段；401 原样
/// 透出（编排层对 `supports_refresh = false` 的家不重试，直接把「90 天重贴」
/// 这条出路交给用户）。
pub async fn query_usage(store: &AccountStore, account_id: &str) -> Result<Value, GatewayError> {
    // 会话读取与目录刷新同一条路（归属校验、启用 / 凭证判据都在那里）：
    // 点名了不存在的账号 → None → 404。
    let entry =
        models::account_session(store, account_id).ok_or_else(|| GatewayError::with_status(404, "账号不存在"))?;

    // 出口：与转发同一条（会话里已解析）。解析失败的账号转发也发不出去，
    // 这里同判同码（`account_proxy_unavailable`），不给一次注定失败的出网。
    if let Some(error) = entry
        .session
        .get("proxyError")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return Err(
            GatewayError::with_status(502, error.to_string()).with_code("account_proxy_unavailable")
        );
    }
    let resolved = session_proxy(&entry.session);
    let proxy: Option<&ResolvedProxy> = resolved.as_ref();

    let token = entry
        .session
        .get("auth")
        .and_then(|auth| auth.get("accessToken"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if token.is_empty() {
        // 走到这里的账号本应有凭证（会话读取已判 has_credentials）；真缺了
        // 说明记录被并发改坏 —— 与转发的同款文案指路「重贴 JWT」。用
        // `usage_not_configured` 那套模板不合适：它假设「余额查询另配一份
        // 凭证」，本家的查询凭证就是账号本身。code 仍是那一条机器可识别
        // 标记，前端照旧显示成中性提示而不是红色失败。
        return Err(GatewayError::with_status(
            400,
            "Free Pi 账号缺少登录令牌：请重新粘贴一枚有效的登录 JWT",
        )
        .with_code(USAGE_NOT_CONFIGURED_CODE));
    }

    let url = format!("{BASE_URL}{ME_STATS_PATH}");
    // 头与 CLI 的 fetchUsageText 逐字对齐：只有 Authorization（不带
    // x-client-version / x-session-id —— 用量接口后面没有那两个闸门）。
    let headers = vec![("authorization".to_string(), format!("Bearer {token}"))];
    let response = send_raw("GET", &url, None, &headers, proxy, Some(REQUEST_TIMEOUT_MS))
        .await
        .map_err(|error| {
            if error.is_timeout() {
                GatewayError::with_status(504, "Free Pi 余额接口超时")
            } else {
                GatewayError::with_status(502, format!("Free Pi 余额接口请求失败: {error}"))
            }
        })?;
    if !response.ok {
        // 401 原样透出 + 指路「重贴」（编排层对无续期家的 401 不重试）；
        // 其余状态带上游 code（errors.ts 的固定枚举，文本可信）不带原文
        // —— 余额错误文案是直接进界面的，不透传任意上游文本。
        let code = response
            .payload
            .as_ref()
            .and_then(|payload| payload.get("code"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(|text| format!("（上游 code: {text}）"))
            .unwrap_or_default();
        return Err(match response.status {
            401 => GatewayError::with_status(
                401,
                "Free Pi 登录令牌无效或已过期（令牌 90 天有效）：请重新完成浏览器登录，\
                 然后在本页重新粘贴新令牌",
            ),
            403 => GatewayError::with_status(
                403,
                format!("Free Pi 余额接口被上游拒绝（HTTP 403）{code}"),
            ),
            _ => GatewayError::with_status(
                502,
                format!("Free Pi 余额接口返回 HTTP {}{code}", response.status),
            ),
        });
    }
    let payload = response
        .payload
        .ok_or_else(|| GatewayError::with_status(502, "Free Pi 余额接口响应不是有效 JSON"))?;
    parse_stats(&payload).ok_or_else(|| {
        GatewayError::with_status(502, "Free Pi 余额响应结构异常（读不出免费额度或积分字段）")
    })
}

/// 今日已用百分比（逐条对照 CLI `usage-tool.ts::percentUsed`）：
/// cap 非正（含缺失 / 0）按 100 —— 没有额度的账号就是「用满了」；
/// 其余四舍五入取整并夹在 0~100。这个函数是 `*_usd` 字段唯一的去处。
pub fn percent_used(spent_usd: f64, cap_usd: f64) -> i64 {
    if !(cap_usd > 0.0) {
        return 100;
    }
    ((spent_usd / cap_usd) * 100.0).round().clamp(0.0, 100.0) as i64
}

/// 取数字：认数字与「数字字符串」（与 `custom::balance_parse` 同一宽容口径；
/// 本家响应是 zod 校验过的数字，这里只做防御）。
fn number_of(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(number) => number.as_f64().filter(|value| value.is_finite()),
        Value::String(text) => text
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite()),
        _ => None,
    }
}

/// 对象里某个键的数字
fn number_at(map: &Map<String, Value>, key: &str) -> Option<f64> {
    number_of(map.get(key))
}

/// `refill_at`（ISO 8601，下一 UTC 零点）→ 毫秒时刻；解析不出 None（不猜）。
fn refill_at_to_ms(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|time| time.timestamp_millis())
        .filter(|ms| *ms > 0)
}

/// 重置时刻 → 本机时区 `MM-DD HH:MM`（与 `custom::balance_parse::reset_text`
/// 同口径：两个模块各留一份，不为一句复用把私有函数升成 crate 可见）。
fn reset_text(ms: i64) -> Option<String> {
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|at| at.format("%m-%d %H:%M").to_string())
}

/// 整数千分位（与上游 `toLocaleString("en-US")` 同形：credits 按惯例是
/// 整数读数，展示串对齐 CLI 的 "12,400 / 15,000" 形态）。
fn thousands(value: i64) -> String {
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    if negative {
        format!("-{grouped}")
    } else {
        grouped
    }
}

/// credits 读数的展示串：整数走千分位，带小数（异常服务端）保留两位。
fn credits_text(value: f64) -> String {
    let rounded = value.round();
    if (value - rounded).abs() < 0.005 {
        thousands(rounded as i64)
    } else {
        format!("{value:.2}")
    }
}

/// 上游响应的**脱敏副本**：USD 计价字段全部剥掉（见模块头「raw 脱敏」）。
/// 其余字段原样保留（tier / lane / refill_at / token 数 / lifetime 的
/// 次数与 token，都是排障要看的）。
fn sanitized_raw(payload: &Value) -> Value {
    let mut raw = payload.clone();
    if let Some(object) = raw.as_object_mut() {
        for key in ["cap_usd_today", "spent_usd_today", "remaining_usd_today", "credit_usd"] {
            object.remove(key);
        }
        if let Some(lifetime) = object.get_mut("lifetime").and_then(Value::as_object_mut) {
            lifetime.remove("spent_usd");
        }
    }
    raw
}

/// 一行明细（balance 为 None 不产出 ——「缺键不写、null 不是 0」的全仓纪律）。
fn wallet(kind: &str, display_name: &str, balance: f64, unit: &str, view: Option<String>) -> Value {
    let mut row = Map::new();
    row.insert("type".to_string(), Value::String(kind.to_string()));
    row.insert("displayName".to_string(), Value::String(display_name.to_string()));
    row.insert("balance".to_string(), json!(balance));
    row.insert("unit".to_string(), Value::String(unit.to_string()));
    if let Some(view) = view {
        row.insert("balanceView".to_string(), Value::String(view));
    }
    Value::Object(row)
}

/// `GET /me/stats` 响应 → 归一化读数（形状契约见
/// `ProviderAdapter::query_usage` 文档）。读不出任何余额读数（credits 字段
/// 与百分比字段全缺）才算结构异常 —— 旧服务端可能只给 `*_usd` 三件套，
/// 那时按 CLI 的回退链展示百分比。
pub(super) fn parse_stats(payload: &Value) -> Option<Value> {
    let stats = payload.as_object()?;

    let remaining_free = number_at(stats, "remaining_credits_today");
    let allowance = number_at(stats, "allowance_credits");
    let credit = number_at(stats, "credit_credits");
    let spent = number_at(stats, "spent_usd_today");
    let cap = number_at(stats, "cap_usd_today");
    let refill_at = stats
        .get("refill_at")
        .and_then(Value::as_str)
        .and_then(refill_at_to_ms);

    // 百分比：*_usd 字段唯一的去处（见模块头「展示口径」）
    let used_percent = match (spent, cap) {
        (Some(spent), Some(cap)) => Some(percent_used(spent, cap) as f64),
        _ => None,
    };
    let remaining_percent = used_percent.map(|used| (100.0 - used).clamp(0.0, 100.0));

    // 主读数：今日免费剩余 + 购买余额（口径见模块头「available 的口径」）
    let available = match (remaining_free, credit) {
        (Some(free), Some(credit)) => Some(free + credit),
        (Some(free), None) => Some(free),
        (None, Some(credit)) => Some(credit),
        (None, None) => None,
    };
    if available.is_none() && used_percent.is_none() {
        return None;
    }

    // 紧凑展示串（余额列只有几十像素宽）：credits 形态优先，退百分比
    // （CLI 的回退链同序：formatCreditsLeftLine → percentUsed 那行）
    let available_view = match (remaining_free, allowance) {
        (Some(remaining), Some(allowance)) => Some(format!(
            "{} / {} credits",
            credits_text(remaining),
            credits_text(allowance)
        )),
        _ => used_percent
            .map(|used| format!("今日已用 {}%", used as i64))
            .or_else(|| credit.map(|credit| format!("{} credits", credits_text(credit)))),
    };

    // 明细行：今日免费额度 + 购买积分（与 CLI formatStats 的两行同源）
    let mut wallets: Vec<Value> = Vec::new();
    if let Some(remaining) = remaining_free {
        let mut parts: Vec<String> = Vec::new();
        parts.push(match allowance {
            Some(allowance) => format!(
                "剩 {} / {} credits",
                credits_text(remaining),
                credits_text(allowance)
            ),
            None => format!("剩 {} credits", credits_text(remaining)),
        });
        if let Some(used) = used_percent {
            parts.push(format!("已用 {}%", used as i64));
        }
        if let Some(text) = refill_at.and_then(reset_text) {
            parts.push(format!("{text} 重置"));
        }
        wallets.push(wallet(
            "free_today",
            "今日免费额度",
            remaining,
            "credits",
            Some(parts.join(" · ")),
        ));
    }
    if let Some(credit) = credit {
        wallets.push(wallet(
            "credit",
            "购买积分",
            credit,
            "credits",
            Some(format!("{} credits", credits_text(credit))),
        ));
    }

    // 逐窗口读数：本家只有一个日窗口（UTC 零点回满），一窗一项
    let mut document = json!({
        "available": available,
        "unit": "credits",
        "wallets": wallets,
        "subscription": Value::Null,
        "raw": sanitized_raw(payload),
    });
    if let Some(object) = document.as_object_mut() {
        let mut window = Map::new();
        window.insert("key".to_string(), Value::String("daily".to_string()));
        window.insert("label".to_string(), Value::String("今日免费额度".to_string()));
        window.insert("window".to_string(), Value::String("daily".to_string()));
        if let Some(remaining) = remaining_percent {
            window.insert("remainingPercent".to_string(), json!(remaining));
            window.insert("view".to_string(), Value::String(format!("剩 {}%", remaining as i64)));
        }
        if let Some(used) = used_percent {
            window.insert("usedPercent".to_string(), json!(used));
        }
        if let Some(resets) = refill_at {
            window.insert("resetsAt".to_string(), Value::from(resets));
        }
        object.insert("windows".to_string(), Value::Array(vec![Value::Object(window)]));
        if let Some(view) = available_view {
            object.insert("availableView".to_string(), Value::String(view));
        }
    }
    Some(document)
}

#[cfg(test)]
mod tests {
    use super::super::ID;
    use super::*;
    use serde_json::json;

    /// 一份完整的 `/me/stats` 响应（MeStatsResponseSchema 全字段；
    /// `*_usd` 值取的是展示口径的合理量级，仅用于验证「算百分比、不展示」）。
    fn full_stats() -> Value {
        json!({
            "user_id": "987654321",
            "handle": "octocat",
            "tier": "established",
            "cap_usd_today": 0.375,
            "spent_usd_today": 0.0625,
            "remaining_usd_today": 0.3125,
            "credit_usd": 1.25,
            "credit_credits": 16667,
            "allowance_credits": 15000,
            "remaining_credits_today": 12400,
            "refill_at": "2026-11-13T00:00:00.000Z",
            "lane": "standard",
            "bonus_credits": 2000,
            "request_count_today": 12,
            "prompt_tokens_today": 3400,
            "completion_tokens_today": 890,
            "lifetime": {
                "spent_usd": 4.2,
                "prompt_tokens": 90000,
                "completion_tokens": 21000,
                "request_count": 300
            }
        })
    }

    /// percent_used 逐条对照 CLI 的 percentUsed：
    /// cap 非正 → 100；正常四舍五入；两侧夹紧。
    #[test]
    fn percent_used_mirrors_upstream_formula() {
        assert_eq!(percent_used(0.0625, 0.375), 17, "6.25/37.5 = 16.67% → 17");
        assert_eq!(percent_used(0.0, 0.375), 0, "没用就是 0%");
        assert_eq!(percent_used(0.5, 0.375), 100, "超用夹到 100");
        assert_eq!(percent_used(-1.0, 0.375), 0, "负数夹到 0");
        assert_eq!(percent_used(0.1, 0.0), 100, "cap 为 0 按用满处理");
        assert_eq!(percent_used(0.1, -1.0), 100, "cap 为负同样按用满处理");
    }

    /// 完整响应 → 归一化读数：主读数 = 免费剩余 + 购买余额；两行明细；
    /// 一个日窗口带重置时刻；美元字段与 `$` 绝不出现在整份文档里。
    #[test]
    fn parse_stats_full_shape_uses_percent_and_credits_only() {
        let document = parse_stats(&full_stats()).expect("完整响应应当解析得出");

        // 主读数：12400（免费剩余）+ 16667（购买）= 29067
        assert_eq!(document.get("available"), Some(&json!(29067.0)));
        assert_eq!(document.get("unit"), Some(&json!("credits")));
        assert_eq!(
            document.get("availableView").and_then(Value::as_str),
            Some("12,400 / 15,000 credits"),
            "紧凑串走 credits 形态（CLI 的 formatCreditsLeftLine 同形）"
        );

        // 明细两行：免费（带已用百分比与重置时刻）+ 购买
        let wallets = document.get("wallets").and_then(Value::as_array).expect("wallets 数组");
        assert_eq!(wallets.len(), 2, "免费 + 购买两行；bonus 与 *_usd 不成行");
        assert_eq!(wallets[0].get("type").and_then(Value::as_str), Some("free_today"));
        assert_eq!(wallets[0].get("balance"), Some(&json!(12400.0)));
        let view = wallets[0].get("balanceView").and_then(Value::as_str).unwrap_or("");
        assert!(view.contains("剩 12,400 / 15,000 credits"), "剩余串：{view}");
        assert!(view.contains("已用 17%"), "百分比串：{view}");
        assert!(view.ends_with("重置"), "重置时刻：{view}");
        assert_eq!(wallets[1].get("type").and_then(Value::as_str), Some("credit"));
        assert_eq!(wallets[1].get("balance"), Some(&json!(16667.0)));

        // 日窗口：剩余 83% / 已用 17% / 重置时刻
        let windows = document.get("windows").and_then(Value::as_array).expect("windows 数组");
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].get("remainingPercent"), Some(&json!(83.0)));
        assert_eq!(windows[0].get("usedPercent"), Some(&json!(17.0)));
        assert_eq!(
            windows[0].get("resetsAt"),
            Some(&json!(
                chrono::DateTime::parse_from_rfc3339("2026-11-13T00:00:00.000Z")
                    .expect("测试时刻")
                    .timestamp_millis()
            ))
        );

        // 红线：整份文档（含 raw）不出现美元 —— 没有 `$`，也没有任何 _usd 键
        let serialized = document.to_string();
        assert!(!serialized.contains('$'), "文档不得含美元符号：{serialized}");
        assert!(!serialized.contains("_usd"), "文档不得含 USD 计价键：{serialized}");
        let raw = document.get("raw").and_then(Value::as_object).expect("raw 对象");
        assert!(raw.get("tier").is_some() && raw.get("lane").is_some(), "非 USD 字段保留");
        assert!(raw.get("cap_usd_today").is_none(), "cap_usd_today 必须剥掉");
        assert!(raw.get("credit_usd").is_none(), "credit_usd 必须剥掉");
        let lifetime = raw.get("lifetime").and_then(Value::as_object).expect("lifetime 对象");
        assert!(lifetime.get("spent_usd").is_none(), "lifetime.spent_usd 必须剥掉");
        assert!(lifetime.get("request_count").is_some(), "lifetime 次数保留");
    }

    /// 旧服务端（只有 `*_usd` 三件套、没有 credits 字段）→ 百分比回退链：
    /// available 为 null、availableView 是百分比短语、窗口只剩比例。
    #[test]
    fn parse_stats_percent_fallback_for_legacy_server() {
        let legacy = json!({
            "user_id": "987654321",
            "handle": "octocat",
            "tier": "new",
            "cap_usd_today": 0.375,
            "spent_usd_today": 0.25,
            "remaining_usd_today": 0.125,
            "credit_usd": 0.0,
            "request_count_today": 3,
            "prompt_tokens_today": 120,
            "completion_tokens_today": 40,
            "lifetime": { "spent_usd": 0.25, "prompt_tokens": 120, "completion_tokens": 40, "request_count": 3 }
        });
        let document = parse_stats(&legacy).expect("百分比三件套应当解析得出");
        assert_eq!(document.get("available"), Some(&Value::Null), "没有 credits 字段不给编造的主读数");
        assert_eq!(
            document.get("availableView").and_then(Value::as_str),
            Some("今日已用 67%"),
            "回退链与 CLI 同形：percentUsed 那行"
        );
        let wallets = document.get("wallets").and_then(Value::as_array).expect("wallets 数组");
        assert!(wallets.is_empty(), "读不到数值就不产出行（null 不是 0）");
        let windows = document.get("windows").and_then(Value::as_array).expect("windows 数组");
        assert_eq!(windows[0].get("remainingPercent"), Some(&json!(33.0)));
        let serialized = document.to_string();
        assert!(!serialized.contains('$') && !serialized.contains("_usd"), "红线对回退形态同样生效");
    }

    /// 只有购买余额、免费额度字段缺失（极端旧服务端）→ 主读数取购买余额。
    #[test]
    fn parse_stats_falls_back_to_credit_only() {
        let payload = json!({ "credit_credits": 5000, "tier": "new" });
        let document = parse_stats(&payload).expect("购买余额可读就应当给出读数");
        assert_eq!(document.get("available"), Some(&json!(5000.0)));
        assert_eq!(
            document.get("availableView").and_then(Value::as_str),
            Some("5,000 credits")
        );
    }

    /// 结构异常：既没有 credits 字段也没有百分比三件套 → None（502）。
    #[test]
    fn parse_stats_rejects_unrecognizable_payload() {
        assert!(parse_stats(&json!({ "handle": "octocat" })).is_none());
        assert!(parse_stats(&json!("not an object")).is_none());
        assert!(parse_stats(&Value::Null).is_none());
    }

    /// 千分位与 credits 展示串：整数分组、负号保留、小数走两位。
    #[test]
    fn credits_text_groups_thousands() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(12400), "12,400");
        assert_eq!(thousands(1_500_000), "1,500,000");
        assert_eq!(thousands(-1234), "-1,234");
        assert_eq!(credits_text(12400.0), "12,400");
        assert_eq!(credits_text(12400.5), "12400.50");
    }

    /// 非法 / 非 RFC3339 的 refill_at 不产出重置时刻，也不让解析失败。
    #[test]
    fn refill_at_is_tolerated_when_missing_or_invalid() {
        let payload = json!({
            "remaining_credits_today": 100,
            "allowance_credits": 200,
            "refill_at": "not-a-timestamp"
        });
        let document = parse_stats(&payload).expect("refill_at 坏了不影响其余读数");
        let windows = document.get("windows").and_then(Value::as_array).expect("windows 数组");
        assert!(windows[0].get("resetsAt").is_none(), "解析不出就不写这一键");
        let view = document
            .get("wallets")
            .and_then(Value::as_array)
            .and_then(|rows| rows.first())
            .and_then(|row| row.get("balanceView"))
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(!view.contains("重置"), "坏时刻不进展示串：{view}");
    }

    /// provider id 常量与注册表一致（防手滑改字面量）。
    #[test]
    fn id_matches_registry() {
        assert_eq!(ID, "free-pi");
        assert_eq!(BASE_URL, "https://api.freepi.ai");
    }
}
