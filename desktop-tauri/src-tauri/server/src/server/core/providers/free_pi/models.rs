//! Free Pi 的模型目录：`GET /client-version` 是权威来源，**另有一份单条
//! 静态兜底**（远程拉不到 / 还没拉过时用）。
//!
//! ── 响应形状（`free-pi-cli/src/vendor/shared/api.ts` + 2026-11-12 实测）──
//! ```json
//! { "min": "0.6.19", "latest": "0.6.19",
//!   "min_desktop_version": "0.0.0", "latest_desktop_version": "0.0.0",
//!   "model": "DeepSeek V4 Flash",
//!   "models": [ { "id": "deepseek-v4-flash",
//!                 "name": "DeepSeek V4 Flash · 🔥 Free Unlimited: limited time",
//!                 "display_name": "DeepSeek V4 Flash", "free": true,
//!                 "usage_x": null, "note": null } ] }
//! ```
//! schema 只认 `models[].{id,name}`（`name` 是 picker label），其余字段
//! （`display_name` / `free` / `*_desktop_version` / `notice`）连 CLI 的
//! `ClientVersionResponseSchema` 都不读 —— 本模块照抄同一边界：多出来的键
//! 原样忽略，不猜它们的语义。
//!
//! `models[]` 可选（旧服务端可以不带，schema 是 `.optional()`）；CLI 的
//! 回退链是 `models[]` → 单值 `model` 字段（「the active model name」）→
//! （CLI 侧无目录）。本模块照抄这条回退链，两级都没有才算「这次没拉到」。
//! 条目缺 `id` 或 `id` 空白的丢掉；`name` 缺时用 `id` 补（CLI 的 picker
//! label 语义同源）。
//!
//! ── 缓存与重启 ──────────────────────────────────────────────
//! 远程清单进进程缓存（`OnceLock<Mutex<…>>`）+ 落盘缓存
//! （`catalog_cache::save(SCOPE_FREE_PI, …)`）；首次访问时从落盘缓存装回
//! —— 进程重启不清零，这是 `catalog_cache` 模块头的同一套约定。缓存
//! **不带有效期**（宁可给一份旧清单，也不退回静态兜底）；自动刷新路径的
//! TTL 早退只管「别反复打上游」，与缓存无关。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：零 unwrap/expect/panic；持锁期间不打日志
//! （`Db::with` 与 `logging` 共用一把库锁，`std::sync::Mutex` 不可重入）。

use std::sync::{Mutex, OnceLock};

use serde_json::{json, Value};

use crate::server::core::account_store::state::CurrentEntry;
use crate::server::core::account_store::AccountStore;
use crate::server::core::auth_http::send_raw;
use crate::server::core::providers::adapter::ModelRefreshOutcome;
use crate::server::core::providers::catalog_cache;
use crate::server::errors::GatewayError;
use crate::server::logging;

use super::{BASE_URL, CLIENT_VERSION, CLIENT_VERSION_PATH, ID};

/// 本家在 `catalog_cache` 里的 scope 名（登记见 `catalog_cache::ALL_SCOPES`）。
pub const SCOPE: &str = catalog_cache::SCOPE_FREE_PI;

/// 自动刷新路径的目录缓存时长（自动路径不反复打上游；手动「获取模型」
/// 走 `force = true` 绕过，与 LobsterAI 的 2 小时同档）。
const CATALOG_TTL_MS: i64 = 2 * 60 * 60 * 1000;

/// 目录请求超时（CLI 对同一端点用 3 秒短超时做启动检查；这里是一次
/// 维护动作，放宽到 10 秒，与别家目录超时同档）。
const CATALOG_TIMEOUT_MS: u64 = 10_000;

/// 静态兜底模型（`free-pi-cli/src/provider.ts::MODEL_ID`）。
pub const FALLBACK_MODEL_ID: &str = "deepseek/deepseek-v4-flash";

/// 进程内目录状态：**只装远程清单**（空 = 还没拉到过，对外用静态兜底）。
/// 条目是**归一后的 `(id, name)` 对**——对外形态在 [`list`] 时统一由
/// [`to_manifest_value`] 套上，刷新路径与重启恢复路径因此不会分叉。
#[derive(Clone, Debug, Default)]
struct CatalogState {
    entries: Vec<(String, String)>,
    fetched_at: i64,
}

static STATE: OnceLock<Mutex<Option<CatalogState>>> = OnceLock::new();

/// 取状态句柄（毒锁按「可重建的展示数据」处理，与 LobsterAI 目录同口径）。
fn state() -> &'static Mutex<Option<CatalogState>> {
    STATE.get_or_init(|| Mutex::new(None))
}

/// 在锁内执行一段取值；首次访问时把落盘缓存装回内存（重启恢复路径）。
fn with_state<R>(f: impl FnOnce(&mut Option<CatalogState>) -> R) -> R {
    let mut guard = match state().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.is_none() {
        if let Some(cached) = catalog_cache::load(SCOPE) {
            let entries: Vec<(String, String)> = cached
                .models
                .iter()
                .filter_map(entry_from_cached)
                .collect();
            if !entries.is_empty() {
                *guard = Some(CatalogState {
                    entries,
                    fetched_at: cached.fetched_at,
                });
            }
        }
    }
    f(&mut guard)
}

/// 从落盘缓存条目还原（缓存里存的就是 [`to_cached_value`] 写下的形态）。
fn entry_from_cached(value: &Value) -> Option<(String, String)> {
    let id = value.get("id").and_then(Value::as_str)?.trim().to_string();
    if id.is_empty() {
        return None;
    }
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&id)
        .to_string();
    Some((id, name))
}

/// 静态兜底目录：**单条**，字段逐项对照 CLI 的 provider 配置
/// （`free-pi-cli/src/provider.ts::buildProviderConfig`）：
///   · `reasoning: false`、`input: ["text"]` → 思考与图片都不声称支持；
///   · `contextWindow: 1_048_576` / `maxTokens: 8192` 是 CLI 自己下发的
///     展示值（provider config 字段），照抄；
///   · `supportsToolCall` **不写**：CLI 的 provider 配置里没有任何工具
///     声明，代理是否透传 `tools` 没有实测证据 —— 宁可少报，不编一个 true。
///
/// 只在「远程没拉到过 / 拉不到」时对外；远程一旦成功就是权威。
///
/// ── 兜底 id 与线上目录的差异（2026-11-12 实测，别当没看见）────────
/// 兜底用的是 CLI 常量 `MODEL_ID = "deepseek/deepseek-v4-flash"`
/// （带 `vendor/` 前缀，0.4.10 那代的写法）。同日实测
/// `GET https://api.freepi.ai/client-version` 的 `models[].id` 已经是
/// **不带前缀**的 `deepseek-v4-flash` / `deepseek-v4.1-flash` 等。兜底 id
/// 因此只保证「有一行可展示、请求里 `model` 字段非空」，**不保证上游一定
/// 受理**；正常路径是刷新一次目录（`refresh` 成功即被远程清单覆盖）。这一条
/// 写在这里是为了下次有人对着兜底 id 排查 400 时不用重新发现一遍。
fn fallback_models() -> Vec<Value> {
    vec![json!({
        "id": FALLBACK_MODEL_ID,
        "name": FALLBACK_MODEL_ID,
        "maxInputTokens": 1_048_576,
        "maxOutputTokens": 8192,
        "supportsImages": false,
        "supportsVideo": false,
        "supportsReasoning": false,
    })]
}

/// 远程条目 → 对外目录条目（键名对照本仓目录聚合层的惯例，见
/// LobsterAI `to_manifest_value` 同款键集）。
fn to_manifest_value(id: &str, name: &str) -> Value {
    json!({
        "id": id,
        "name": name,
        "maxInputTokens": 1_048_576,
        "maxOutputTokens": 8192,
        "supportsImages": false,
        "supportsVideo": false,
        "supportsReasoning": false,
    })
}

/// 落盘缓存里的条目形态（只存上游给的 `{id, name}`，不掺展示字段 ——
/// 恢复时 [`to_manifest_value`] 会重新套上展示字段，两处不会分叉）。
fn to_cached_value(id: &str, name: &str) -> Value {
    json!({ "id": id, "name": name })
}

/// 当前对外目录：**远程优先，没有则静态兜底**（对外形态在这里统一组装，
/// 刷新路径与重启恢复路径共用同一张映射，不会分叉）。
pub fn list() -> Vec<Value> {
    with_state(|guard| match guard.as_ref() {
        Some(state) if !state.entries.is_empty() => state
            .entries
            .iter()
            .map(|(id, name)| to_manifest_value(id, name))
            .collect(),
        _ => fallback_models(),
    })
}

/// 目录最后一次落地时刻（毫秒；0 = 没有远程目录）。
pub fn last_refreshed_at() -> i64 {
    with_state(|guard| guard.as_ref().map(|state| state.fetched_at).unwrap_or(0))
}

/// 解析 `/client-version` 响应里的目录：`models[]` → 单值 `model` → None
/// （CLI `update-check.ts` 的回退链，两级都没有才算失败）。
pub fn parse_models(payload: &Value) -> Option<Vec<(String, String)>> {
    if let Some(list) = payload.get("models").and_then(Value::as_array) {
        let parsed: Vec<(String, String)> = list
            .iter()
            .filter_map(|item| {
                let id = item.get("id").and_then(Value::as_str)?.trim().to_string();
                if id.is_empty() {
                    return None;
                }
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(&id)
                    .to_string();
                Some((id, name))
            })
            .collect();
        if !parsed.is_empty() {
            return Some(parsed);
        }
    }
    // 旧服务端兼容：单值 `model` 字段（active model 的名字）
    payload
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(|id| vec![(id.to_string(), id.to_string())])
}

/// 刷新用的账号会话：空 `account_id` = 该家队首可用账号；点名了就按 id
/// 直取且**校验归属**（选了 A、用的是 B 的静默错误不能有）。
pub fn account_session(store: &AccountStore, account_id: &str) -> Option<CurrentEntry> {
    let requested = account_id.trim();
    if requested.is_empty() {
        return store.current_entry_for_provider(ID);
    }
    if store.account_provider_of(requested).as_deref() != Some(ID) {
        return None;
    }
    store.session_for_account(requested)
}

/// 拉一次目录（`GET {BASE_URL}/client-version`，JWT + 伪装头与对话同源）。
async fn fetch_catalog(jwt: &str) -> Result<Vec<(String, String)>, GatewayError> {
    let url = format!("{BASE_URL}{CLIENT_VERSION_PATH}");
    let headers = vec![
        (
            "authorization".to_string(),
            format!("Bearer {jwt}"),
        ),
        ("accept".to_string(), "application/json".to_string()),
        // 伪装头与对话路径同一套（服务端两个闸门读同一组头，见模块头）
        ("x-client-version".to_string(), CLIENT_VERSION.to_string()),
    ];
    let response = send_raw("GET", &url, None, &headers, None, Some(CATALOG_TIMEOUT_MS))
        .await
        .map_err(|error| {
            if error.is_timeout() {
                GatewayError::with_status(504, "Free Pi 目录接口超时")
            } else {
                GatewayError::with_status(502, format!("Free Pi 目录接口请求失败: {error}"))
            }
        })?;
    let payload = response
        .payload
        .ok_or_else(|| GatewayError::with_status(502, "Free Pi 目录接口响应不是 JSON"))?;
    if !response.ok {
        return Err(GatewayError::with_status(
            i32::from(response.status),
            format!("Free Pi 目录接口返回 HTTP {}", response.status),
        ));
    }
    parse_models(&payload).ok_or_else(|| {
        GatewayError::with_status(502, "Free Pi 目录响应里没有可用的 models 清单")
    })
}

/// 刷新目录：远程成功即权威；失败时**保留现有清单**并如实汇报原因。
///
/// 「一个账号都没有」不是失败 —— 那是「用户还没添加账号」的正常状态，
/// 返回 `unchanged()`（脚本 / CI 不该看到一个红色失败，与 LobsterAI /
/// AtomCode 同口径）。
pub async fn refresh(store: &AccountStore, account_id: &str, force: bool) -> ModelRefreshOutcome {
    if !force && remote_refreshed() && age_ms() < CATALOG_TTL_MS {
        return ModelRefreshOutcome::unchanged();
    }
    let Some(session) = account_session(store, account_id) else {
        if account_id.trim().is_empty() {
            logging::verbose("[Models]", "Free Pi 模型目录刷新跳过：尚未添加账号");
            return ModelRefreshOutcome::unchanged();
        }
        return ModelRefreshOutcome::failed("指定的账号不存在或不属于 Free Pi，请重新选择");
    };
    let jwt = session
        .session
        .get("auth")
        .and_then(|auth| auth.get("accessToken"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    if jwt.is_empty() {
        return ModelRefreshOutcome::failed("Free Pi 账号缺少登录令牌，请重新粘贴 JWT");
    }

    match fetch_catalog(&jwt).await {
        Ok(entries) if !entries.is_empty() => {
            let fetched_at = logging::now_ms();
            let cached: Vec<Value> = entries
                .iter()
                .map(|(id, name)| to_cached_value(id, name))
                .collect();
            catalog_cache::save(SCOPE, &cached, fetched_at);
            let count = entries.len();
            with_state(|guard| {
                *guard = Some(CatalogState {
                    entries,
                    fetched_at,
                });
            });
            logging::log("[Models]", &format!("Free Pi 目录刷新完成：{count} 个模型"));
            ModelRefreshOutcome::refreshed(count)
        }
        Ok(_) => ModelRefreshOutcome::failed("Free Pi 目录为空（上游未返回可用模型）"),
        Err(error) => {
            logging::verbose(
                "[Models]",
                &format!("Free Pi 目录刷新失败，沿用现有清单：{}", error.message),
            );
            ModelRefreshOutcome::failed(error.message)
        }
    }
}

/// 有没有**远程拉到的**清单（false 时 [`list`] 给的是静态兜底）。
/// `catalog::refresh_meta` 对 Free Pi 的 remote 位取这里。
pub(crate) fn remote_refreshed() -> bool {
    with_state(|guard| {
        guard
            .as_ref()
            .map(|state| state.fetched_at > 0 && !state.entries.is_empty())
            .unwrap_or(false)
    })
}

/// 当前远程目录的年龄（毫秒；没有远程目录时返回 0 —— 会被 TTL 判定
/// 当成「刚刷过」，但那一支先要求 `remote_refreshed()`，0 年龄走不到）。
fn age_ms() -> i64 {
    with_state(|guard| {
        guard
            .as_ref()
            .map(|state| logging::now_ms() - state.fetched_at)
            .unwrap_or(0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `models[]` 解析：id/name 照读、name 缺失用 id 补、空 id 丢掉；
    /// 旧服务端回退到单值 `model` 字段；两级都没有返回 None。
    #[test]
    fn parse_models_follows_cli_fallback_chain() {
        let full = json!({
            "min": "0.4.0", "latest": "0.4.10",
            "models": [
                { "id": "deepseek/deepseek-v4-flash", "name": "DeepSeek V4 Flash" },
                { "id": "  kimi-k3  " },
                { "name": "缺 id 的条目要被丢掉" },
                { "id": "" }
            ]
        });
        let parsed = parse_models(&full).expect("models[] 应当解析得出");
        assert_eq!(
            parsed,
            vec![
                ("deepseek/deepseek-v4-flash".to_string(), "DeepSeek V4 Flash".to_string()),
                ("kimi-k3".to_string(), "kimi-k3".to_string()),
            ]
        );

        let legacy = json!({ "min": "0.1.0", "latest": "0.1.2", "model": "deepseek/deepseek-v4-flash" });
        assert_eq!(
            parse_models(&legacy).map(|list| list.into_iter().map(|(id, _)| id).collect::<Vec<_>>()),
            Some(vec!["deepseek/deepseek-v4-flash".to_string()]),
            "旧服务端只有单值 model 字段时按 CLI 的回退链取它"
        );

        assert!(parse_models(&json!({ "min": "0.4.0", "latest": "0.4.10" })).is_none());
        assert!(parse_models(&json!({ "models": [{ "name": "no id" }] })).is_none());
        assert!(parse_models(&json!("not an object")).is_none());
    }

    /// 静态兜底：单条 `deepseek/deepseek-v4-flash`，远程不可达时对外
    /// 就是它（验收口径），且不声称未核实的工具/图片/思考能力。
    #[test]
    fn fallback_lists_static_model_when_remote_unreachable() {
        let list = list();
        assert_eq!(list.len(), 1, "兜底清单只有 CLI 声明的那一条");
        assert_eq!(
            list[0].get("id").and_then(Value::as_str),
            Some(FALLBACK_MODEL_ID)
        );
        let serialized = list[0].to_string();
        assert!(
            !serialized.contains("supportsToolCall"),
            "工具透传没有证据，不允许编造 supportsToolCall"
        );
    }
}
