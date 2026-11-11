//! 模型目录：静态兜底（文档快照）+ 远程清单（V2 `GET /v2/models`）+ 别名解析。
//!
//! ── 清单来源与判据 ──────────────────────────────────────────
//！ · 远程：官方 `GET https://qianfan.baidubce.com/v2/models`（API Key 鉴权，
//!   文档 https://cloud.baidu.com/doc/qianfan-api/s/Dmba8k71y）。条目带
//!   `type`（chat/text2image/…）、`endpoints`、`prompt_tokens`/`max_tokens`
//!   与 `features` —— 本插件只收 `type == "chat"` 且 `endpoints` 含
//!   `/v2/chat/completions`、`status != "offline"` 的条目，其余如实丢弃
//!   （生图/向量/重排序不是对话模型，混进清单会变成「点得动但必 400」）。
//!   `features` 含 `function_call` 才给 `supportsToolCall`，不按 id 猜。
//! · 静态兜底（V2）：千帆「模型列表」文档 2026-11-11 快照的文本生成与深度
//!   思考两节（https://cloud.baidu.com/doc/qianfan/s/rmh4stp0j）—— ERNIE 5.x
//!   旗舰 + DeepSeek-V4 系 + Qwen3.5 系 + GLM-5.x 系。k 值按 1024 折算。
//! · 静态兜底（V1 旧版）：wenxinworkshop 的对话接入点后缀（即 model 名），
//!   以 new-api 百度渠道 adaptor 维护的历史后缀为对照
//!   （research/wenxin-reverse-refs-2026-11-12.md §A 已核）。旧版没有目录
//!   接口 —— 这是「上游没有」，不是「还没做」。
//!
//! ── 别名（-thinking / -nothinking 后缀）─────────────────────
//! 思考开关是 V2 请求体上的 `thinking.type`（enabled/disabled，官方文本生成
//! 文档「thinking 参数」节）。网关的模型规则本来就能做「别名 → 模型 + 思考
//! 档位」映射；这里再兜一层：请求的模型名带 `-nothinking` / `-thinking`
//! 后缀、且剥掉后缀的 id 在目录里能认出来，就剥后缀并显式设 thinking。
//! 剥完不认识就原样透传 —— 让上游的 400 如实报错，不猜。

use std::sync::{Mutex, OnceLock};

use agent2api_provider_protocol as proto;
use serde_json::{json, Value};

use crate::http;
use super::credentials::Credential;

const TTL_MS: i64 = 10 * 60 * 1000;

struct CatalogState {
    models: Vec<Value>,
    fetched_at: i64,
}

static REMOTE: OnceLock<Mutex<Option<CatalogState>>> = OnceLock::new();

fn remote_slot() -> &'static Mutex<Option<CatalogState>> {
    REMOTE.get_or_init(|| Mutex::new(None))
}

fn with_remote<R>(f: impl FnOnce(&mut Option<CatalogState>) -> R) -> R {
    let mutex = remote_slot();
    let mut guard = mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

/// 静态兜底：API Key（V2）模式。来源与折算口径见模块头。
pub fn static_v2() -> Vec<Value> {
    use super::model_row;
    vec![
        model_row("ernie-5.5-flash-preview", "ERNIE-5.5-Flash-Preview", 1_048_576, 1_048_576, true, true),
        model_row("ernie-5.1", "ERNIE 5.1", 121_856, 65_536, true, true),
        model_row("deepseek-v4.1-flash-1017", "DeepSeek-V4.1-Flash-1017", 1_048_576, 393_216, true, true),
        model_row("deepseek-v4.1-flash", "DeepSeek-V4.1-Flash", 1_048_576, 393_216, true, true),
        model_row("deepseek-v4-pro-0813", "DeepSeek-V4-Pro-0813", 1_048_576, 393_216, true, true),
        model_row("deepseek-v4-pro", "DeepSeek-V4-Pro", 1_048_576, 393_216, true, true),
        model_row("deepseek-v4-flash-0731", "DeepSeek-V4-Flash-0731", 1_048_576, 393_216, true, true),
        model_row("qwen3.5-397b-a17b", "Qwen3.5-397B-A17B", 258_048, 65_536, true, true),
        model_row("qwen3.5-122b-a10b", "Qwen3.5-122B-A10B", 258_048, 65_536, true, true),
        model_row("qwen3.5-35b-a3b", "Qwen3.5-35B-A3B", 258_048, 65_536, true, true),
        model_row("qwen3.5-27b", "Qwen3.5-27B", 258_048, 65_536, true, true),
        model_row("glm-5.3-flash", "GLM-5.3-Flash", 1_048_576, 131_072, true, true),
        model_row("glm-5.3", "GLM-5.3", 1_048_576, 131_072, true, true),
        model_row("glm-5.2", "GLM-5.2", 1_048_576, 131_072, true, true),
        model_row("glm-5.1", "GLM-5.1", 202_752, 131_072, true, true),
    ]
}

/// 静态兜底：AK/SK（V1 旧版）模式 —— 对话接入点后缀即 model 名。
pub fn static_v1() -> Vec<Value> {
    use super::model_row;
    vec![
        model_row("completions_pro", "ERNIE Bot 4.0（旧名）", 0, 0, false, false),
        model_row("ernie-4.0-8k", "ERNIE 4.0 8K", 0, 0, false, false),
        model_row("ernie-4.0-turbo-8k", "ERNIE 4.0 Turbo 8K", 0, 0, false, false),
        model_row("ernie-3.5-8k", "ERNIE 3.5 8K", 0, 0, false, false),
        model_row("ernie-speed-128k", "ERNIE Speed 128K", 0, 0, false, false),
        model_row("ernie-speed-8k", "ERNIE Speed 8K", 0, 0, false, false),
        model_row("ernie-lite-8k", "ERNIE Lite 8K", 0, 0, false, false),
        model_row("ernie-tiny-8k", "ERNIE Tiny 8K", 0, 0, false, false),
    ]
}

/// 当前目录（远程优先，空则静态兜底）。
pub fn list_for(cred: &Credential) -> proto::ModelList {
    let remote = with_remote(|slot| {
        slot.as_ref()
            .filter(|state| !state.models.is_empty())
            .map(|state| (state.models.clone(), state.fetched_at))
    });
    if let Some((models, fetched_at)) = remote {
        return proto::ModelList {
            models,
            meta: proto::CatalogMeta { remote: true, refreshed_at: fetched_at },
        };
    }
    let models = if cred.is_ak_sk() { static_v1() } else { static_v2() };
    proto::ModelList { models, meta: proto::CatalogMeta::default() }
}

/// 拉（或用 TTL 内缓存）一次远程目录。V1 模式没有目录接口 → unchanged。
pub async fn refresh(
    proxy: Option<&proto::WireProxy>,
    gateway_proxy: &str,
    cred: &Credential,
    api_key: &str,
    force: bool,
) -> proto::RefreshOutcome {
    if cred.is_ak_sk() {
        return proto::RefreshOutcome::unchanged();
    }
    let now = http::now_ms();
    if !force {
        let fresh = with_remote(|slot| {
            slot.as_ref()
                .map(|state| !state.models.is_empty() && now - state.fetched_at < TTL_MS)
                .unwrap_or(false)
        });
        if fresh {
            return proto::RefreshOutcome::unchanged();
        }
    }
    let client = http::client(proxy, gateway_proxy);
    let response = match client
        .get(format!("{}/models", cred.chat_base))
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Accept", "application/json")
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => return proto::RefreshOutcome::failed(format!("拉取模型目录失败：{error}")),
    };
    let status = response.status().as_u16();
    let text = match response.text().await {
        Ok(text) => text,
        Err(error) => return proto::RefreshOutcome::failed(format!("读取模型目录失败：{error}")),
    };
    if status == 401 || status == 403 {
        return proto::RefreshOutcome::failed("模型目录鉴权被拒（API Key 无效或未授权该资源）");
    }
    let payload: Value = match serde_json::from_str(&text) {
        Ok(payload) => payload,
        Err(_) => {
            return proto::RefreshOutcome::failed(format!("模型目录不是 JSON（HTTP {status}）"));
        }
    };
    let models = parse_remote(&payload);
    if models.is_empty() {
        return proto::RefreshOutcome::failed("模型目录为空或没有可用的对话模型（保留现有清单）");
    }
    let count = models.len();
    with_remote(|slot| {
        *slot = Some(CatalogState { models: models.clone(), fetched_at: http::now_ms() });
    });
    proto::RefreshOutcome::refreshed(count)
}

/// `/v2/models` 响应 → 网关模型记录。只收对话模型，判据见模块头。
fn parse_remote(payload: &Value) -> Vec<Value> {
    let rows = payload
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut out = Vec::new();
    for row in rows {
        let id = row.get("id").and_then(Value::as_str).map(str::trim).unwrap_or("");
        if id.is_empty() {
            continue;
        }
        if row.get("type").and_then(Value::as_str) != Some("chat") {
            continue;
        }
        if row.get("status").and_then(Value::as_str) == Some("offline") {
            continue;
        }
        let endpoints_chat = row
            .get("endpoints")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|item| item.contains("/v2/chat/completions"))
            })
            .unwrap_or(true); // 旧字段形态不给 endpoints 时不误杀，交给转发实测
        if !endpoints_chat {
            continue;
        }
        let name = row
            .get("model_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(id);
        let tools = row
            .get("features")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|item| item == "function_call")
            })
            .unwrap_or(false);
        let mut model = json!({ "id": id, "name": name, "supportsToolCall": tools });
        if let Some(max_in) = row.get("prompt_tokens").and_then(Value::as_i64) {
            if max_in > 0 {
                model["maxInputTokens"] = json!(max_in);
            }
        }
        if let Some(max_out) = row.get("max_tokens").and_then(Value::as_i64) {
            if max_out > 0 {
                model["maxOutputTokens"] = json!(max_out);
            }
        }
        out.push(model);
    }
    out.sort_by(|left, right| {
        let key = |value: &Value| {
            value
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string()
        };
        key(left).cmp(&key(right))
    });
    out
}

/// 目录里认不认这个 id（静态 + 远程都查）。
fn catalog_contains(id: &str) -> bool {
    let wanted = id.trim().to_lowercase();
    if wanted.is_empty() {
        return false;
    }
    let in_static = static_v2()
        .iter()
        .chain(static_v1().iter())
        .any(|model| model.get("id").and_then(Value::as_str) == Some(wanted.as_str()));
    if in_static {
        return true;
    }
    with_remote(|slot| {
        slot.as_ref()
            .map(|state| {
                state.models.iter().any(|model| {
                    model
                        .get("id")
                        .and_then(Value::as_str)
                        .map(str::to_lowercase)
                        .as_deref()
                        == Some(wanted.as_str())
                })
            })
            .unwrap_or(false)
    })
}

/// 解析请求里的模型名 → (发送名, 思考开关)。
///
/// 后缀只在「剥掉后缀的 id 目录认得出」时才剥（数据驱动，不按家族猜）；
/// 其余情况原样透传。
pub fn resolve_model(requested: &str) -> (String, Option<bool>) {
    let trimmed = requested.trim();
    if let Some(base) = trimmed.strip_suffix("-nothinking") {
        if !base.is_empty() && catalog_contains(base) {
            return (base.to_string(), Some(false));
        }
    }
    if let Some(base) = trimmed.strip_suffix("-thinking") {
        if !base.is_empty() && catalog_contains(base) {
            return (base.to_string(), Some(true));
        }
    }
    (trimmed.to_string(), None)
}
