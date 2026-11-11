//! 文心（百度千帆）插件：**一条 provider 承载两代鉴权面**。
//!
//! ── 上游事实（全部有出处，见 `docs/research/qianfan-ernie-api-surface.md`）──
//!   · **V2（现行，主对接面）**：`POST https://qianfan.baidubce.com/v2/chat/completions`，
//!     OpenAI chat/completions 兼容，`Authorization: Bearer <API Key>`
//!     （`bce-v3/ALTAK-…`，官方鉴权页明写**永久有效**），流式为 SSE，
//!     终止帧字面量 `data: [DONE]`。目录 `GET /v2/models`。
//!     来源：https://cloud.baidu.com/doc/qianfan-api/s/3m7of64lb 、
//!     https://cloud.baidu.com/doc/qianfan-api/s/ym9chdsy5 、
//!     https://cloud.baidu.com/doc/qianfan-api/s/Dmba8k71y
//!   · **V1（旧版，官方已公告即将下线）**：`POST {root}/rpc/2.0/ai_custom/v1/
//!     wenxinworkshop/chat/{endpoint}?access_token=…`，鉴权是 AK/SK 先换
//!     `access_token`（`POST /oauth/2.0/token`，`expires_in=2592000`，30 天），
//!     响应字段是 `result`/`is_end`/`error_code`（**没有** `data: [DONE]`），
//!     业务失败走 **HTTP 200 + `{"error_code":…}`**。
//!     来源：https://ai.baidu.com/ai-doc/WENXINWORKSHOP/jlil56u11 、
//!     https://ai.baidu.com/ai-doc/REFERENCE/Ck3dwjhhu
//!
//! ── 为什么一家承载两种鉴权（而不是拆成两家 provider）─────────────
//! `provider-adaptation` §1.2 的拆家判据是「**账号体系是否分开**」：两套账号、
//! 两份目录、令牌不通用 → 拆。这里不成立 —— 两种模式是**同一家（千帆）的
//! 两代 API**，账号就是同一个百度智能云账号，模型目录也是同一份（V1 只是没有
//! 目录接口）。所以它是**账号上的模式字段**（`secret.mode`），不是 provider 身份。
//! 唯一的实际差别（对话协议与目录接口）由 `credential.mode` 在转发层分叉。
//!
//! ── 能力位口径（逐条给依据，没有一条是「还没做」）───────────────
//!   · `chat = true`：上游就是 chat/completions。
//!   · `stateful = true`：本插件走 `conversation`（自己发请求、自己把帧翻成
//!     OpenAI chat SSE）。V1 不是 OpenAI 协议，必须由插件翻译；V2 虽然同构，
//!     但两条模式共用一条出口比按模式分叉两条路径更不容易分叉出 bug。
//!     网关对声明 `stateful` 的家**恒发 `stream: true`**，非流式由网关自己聚合
//!     （`plugin_host::rpc` 的 `forward_conversation` 注释原文）。
//!   · `refresh = true`：**仅 AK/SK 模式**有可程序化续期的凭证 —— access_token
//!     30 天有效、可随时重取（官方 oauth 页）。API Key 模式没有续期接口，
//!     那是「上游没有」而不是「没做」：官方鉴权页明写 API Key 永久有效、
//!     删 Key 即刻失效、无换发端点。因此 API Key 账号的 `token.expiring`
//!     恒 false（见 `credentials::token_expiring`），后台维护不会白跑。
//!   · `model_refresh = true`：V2 有 `GET /v2/models`。V1 没有目录接口 ——
//!     这是「上游没有」：V1 的模型编码在 URL 路径 `{endpoint}` 里，
//!     官方 v1 文档全文没有清单端点。所以 AK/SK 账号刷新时返回 `unchanged()`。
//!   · `usage = **false**`：**排除过程**（不是没做）——千帆 v2 推理 API 面
//!     没有余额/用量端点：`/v2/models` 只给模型元数据与限流值，官方 API 参考
//!     侧栏的「量包管理 / 算力单元」等计费接口全部要求百度云 **AK/SK 的
//!     bce-auth-v1 签名**（`https://aca.bce.baidu.com/doc/qianfan/s/bmh4suwvf`），
//!     那是另一套凭证族（云账号签名），不是本插件持有的千帆 API Key；
//!     用 API Key 打那些端点必 401。因此不打 ✓，也不给界面入口 ——
//!     一个点了必然报错的按钮比没有更糟。
//!   · `checkin = false`：千帆**没有**签到 / 每日领取 / 新手礼这类活动面
//!     （官方 API 参考侧栏无对应接口，控制台的「量包/代金券」是人工购买与
//!     财务流程，不是可程序化领取的每日动作）。不进签到清单。
//!   · `web_login = false`：没有网页授权换取凭证的链路，凭证是控制台复制的
//!     静态 Key / AK+SK。
//!
//! ── 错误分类（判据取自官方错误码表，2026-11-11 快照）────────────
//! V2 错误信封：`{"error":{"code","message","type"},"id":"as-…"}`，
//! 表见 https://cloud.baidu.com/doc/qianfan-api/s/Om9b4yj3w 。四档归法见
//! [`WenxinProvider::classify`] 的注释 —— 每一条都写了「判错会怎样」。
//!
//! ── 凭证去哪 ────────────────────────────────────────────────
//! 一律账号记录 `secret`（网关落库），定义文件 / pack.json / 日志永不出现。
//! 公开形态只给 `tokenTail`（尾 4 字符）与模式名，与 `provider-adaptation`
//! §2.2 的安全边界同一条红线。

mod credentials;
mod forward;
mod models;

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::ctx::StoreAccount;
use agent2api_provider_sdk::{PluginCtx, Provider, ProviderMeta, SendFuture};
use serde_json::{json, Value};

/// 文心（千帆）。
pub struct WenxinProvider;

/// 模型记录的统一形状（`models.rs` 的两张静态表与远程解析共用）。
///
/// 只用网关认的键名：`maxInputTokens` / `maxOutputTokens` / `supportsToolCall`
/// / `supportsReasoning` / `supportsImages`。写错键名不会报错，只会让能力列
/// 显示成「—」，所以集中在这里一处生成。
pub fn model_row(
    id: &str,
    name: &str,
    max_input: i64,
    max_output: i64,
    tools: bool,
    reasoning: bool,
) -> Value {
    let mut row = json!({
        "id": id,
        "name": name,
        "supportsToolCall": tools,
        "supportsReasoning": reasoning,
        // 千帆 v2 的多模态是「按模型」的能力（`architecture.modality`），
        // 静态表不按 id 猜；远程目录拉到 `input_modalities` 时才置真。
        "supportsImages": false,
    });
    if max_input > 0 {
        row["maxInputTokens"] = json!(max_input);
    }
    if max_output > 0 {
        row["maxOutputTokens"] = json!(max_output);
    }
    row
}

/// 读一条账号记录；`account` 为空串时由网关回落到本家队首账号。
async fn load_account(ctx: &PluginCtx, account: &str) -> Result<StoreAccount, String> {
    ctx.store_get("wenxin", account)
        .await
        .map_err(|error| format!("读取文心账号失败：{error}"))
}

impl Provider for WenxinProvider {
    fn meta(&self) -> ProviderMeta {
        ProviderMeta::new("wenxin", "文心（千帆）")
            .with_capabilities(proto::Capabilities {
                chat: true,
                stateful: true,
                // 仅 AK/SK 模式有可续期的派生凭证（见模块头）
                refresh: true,
                model_refresh: true,
                // 没有余额接口（排除过程见模块头）
                usage: false,
                checkin: false,
                claims: false,
                web_login: false,
                // 帧的 model 名由插件自己改写，网关不必再改一次
                sse_model_rewrite: false,
                sse_strip_newline_chunks: false,
                anonymous_default_session: false,
                system_proxy_when_unset: false,
                refresh_uses_account: false,
                supports_default_model: false,
                env_credentials: false,
            })
            .with_login(vec![proto::LoginMethod::Token {
                label: "千帆 API Key（或 AK + Secret Key）".into(),
                placeholder: "bce-v3/ALTAK-… 开头的 API Key；填 AK/SK 时另填 Secret Key".into(),
                extra_fields: vec![proto::LoginField {
                    key: "secretKey".into(),
                    label: "Secret Key（仅 AK/SK 模式）".into(),
                    placeholder: "填了它就走旧版 AK/SK 模式，留空则按 API Key 模式".into(),
                    optional: true,
                    multiline: false,
                }],
            }])
            // 内置兜底清单用 V2（现行主对接面）；AK/SK 账号的清单由
            // `models::list_for` 按模式换成 V1 接入点名表。
            .with_models(models::static_v2())
            .with_account_features(proto::AccountFeatures {
                usage: false,
                credit_detail: false,
                // 公开视图里表达「这条账号是 API Key 还是 AK/SK」
                identifier: vec!["authMode".to_string()],
                // AK/SK 账号的 access_token 有到期时刻，由 `account_view` 写进来
                expiry: vec!["expiresAt".to_string()],
                long_lived: false,
                ..Default::default()
            })
    }

    // ── 数据面：会话式转发（恒流式出口）─────────────────────────

    fn conversation<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        req: proto::ConversationRequest,
        mut sink: agent2api_provider_sdk::ConversationSink<'a>,
    ) -> SendFuture<'a, ()> {
        Box::pin(async move { forward::forward(ctx, req, &mut sink).await })
    }

    /// 上游错误 → 四档。**判据逐条取自官方错误码表**（见模块头链接）。
    ///
    /// 判错的代价（`provider-adaptation` §2.6）：
    ///   · 限额判成 Fatal → 账号永不轮换，每次撞同一面墙；
    ///   · 凭证失效判成 Fatal → 用户看到 401 而不是自动恢复；
    ///   · 审核误判成限额 → 白扔另一个账号的额度。
    ///
    /// 顺序要紧：**业务细分码先于裸状态码**，否则 403 的三种含义（内容不安全 /
    /// 欠费 / 模型下线）会被一锅端成「权限问题」。
    fn classify(&self, status: u16, body: &Value) -> proto::ErrorClass {
        // ── V1 旧端点：HTTP 200 + error_code（官方 v1 文档原文）──────────
        if let Some(code) = body.get("error_code").and_then(Value::as_i64) {
            return classify_v1(status, code, body);
        }
        // ── V2：{"error":{code,message,type}} ──────────────────────────
        let code = body
            .pointer("/error/code")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let kind = body
            .pointer("/error/type")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        let raw = body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("上游错误");
        let message = format!("上游返回 {status}: {raw}");

        // ① 内容安全（`type: unsafe_request`）：不罚账号。
        //    `system_unsafe` 是**人设/系统提示词**触发，换中性提示词重试有意义；
        //    `user_setting_unsafe` / `functions_unsafe` / `image_url_unsafe` 同理。
        if kind == "unsafe_request"
            || matches!(
                code,
                "system_unsafe"
                    | "user_setting_unsafe"
                    | "functions_unsafe"
                    | "image_url_unsafe"
            )
        {
            return proto::ErrorClass::ContentBlocked {
                status,
                message: format!("{message}{CONTENT_BLOCK_HINT}"),
                upstream_code: None,
            };
        }

        // ② 欠费（403 `account_overdue`）：额度问题，但**充值即恢复**，
        //    所以给短冷却（reset_at: None → 网关 10 分钟兜底），
        //    而不是「冷却到次日零点」—— 后者会让用户充完钱还要等一天。
        if code == "account_overdue" {
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message: format!(
                    "文心账号已欠费（上游：{raw}），充值后立即恢复；\
                     欠费期间该账号不可用"
                ),
                upstream_code: None,
                status,
            };
        }

        // ③ 套餐额度用尽（Coding Plan 的 5 小时 / 周 / 月额度）：额度问题。
        //    窗口长度各不相同且都是滚动窗口，上游不给恢复时刻 → reset_at None。
        if code.starts_with("coding_plan_") && code.ends_with("_quota_exceeded") {
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message: format!("文心账号的套餐额度已用尽（上游：{raw}），额度刷新后恢复"),
                upstream_code: None,
                status,
            };
        }

        // ④ 速率 / 并发限额（429，官方表里 `type` 恒为 `rate_limit_exceeded`）：
        //    分钟级窗口，换账号立刻能继续 → QuotaLimited + 短冷却。
        if status == 429 || kind == "rate_limit_exceeded" {
            return proto::ErrorClass::QuotaLimited {
                reset_at: None,
                message,
                upstream_code: None,
                status,
            };
        }

        // ⑤ 凭证无效（401 `invalid_iam_token`：「bearer token 无效或过期，
        //    或请求没加 Bearer」）：这是唯一该走「刷新后同账号重试」的一条。
        if code == "invalid_iam_token" {
            return proto::ErrorClass::TokenExpired { message };
        }

        // ⑥ 其余 401 / 403：**不是**凭证过期，而是「这把 Key 没有这个权限」
        //    （`invalid_model` 无模型权限 / `invalid_appid` / `no_parameter_permission`
        //    / `model_offline`）。判成 TokenExpired 会让网关白刷一次再撞同一面墙，
        //    文案还会误导用户去重新粘贴一把本来就好好的 Key —— 所以如实 Fatal。
        proto::ErrorClass::Fatal {
            status,
            message,
            upstream_code: None,
        }
    }

    // ── 凭证 ──────────────────────────────────────────────────

    /// 取一枚现在能用的凭证：API Key 模式直接给 Key；AK/SK 模式给 access_token
    /// （缓存新鲜就用，否则换取并比较-再写回）。
    fn ensure_token<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async move {
            let record = load_account(ctx, account).await?;
            let cred = credentials::parse(&record.secret)?;
            if !cred.is_ak_sk() {
                return Ok(cred.api_key);
            }
            if let Some((token, _)) = credentials::cached_v1_token(&record.secret) {
                return Ok(token);
            }
            let (token, expires_at) =
                credentials::fetch_v1_token(None, &ctx.env.proxy, &cred).await?;
            let _ = credentials::save_refreshed_token(ctx, &record, &token, expires_at).await;
            Ok(token)
        })
    }

    /// 401 之后的无条件刷新：AK/SK 模式忽略缓存重取一次；API Key 模式没有
    /// 可刷的东西（上游没有换发端点），返回同一把 Key 让网关如实报错。
    fn refresh_token<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, String> {
        Box::pin(async move {
            let record = load_account(ctx, account).await?;
            let cred = credentials::parse(&record.secret)?;
            if !cred.is_ak_sk() {
                return Ok(cred.api_key);
            }
            let (token, expires_at) =
                credentials::fetch_v1_token(None, &ctx.env.proxy, &cred).await?;
            let _ = credentials::save_refreshed_token(ctx, &record, &token, expires_at).await;
            Ok(token)
        })
    }

    /// 凭证算不算临期（后台维护的入场券）。API Key 模式恒 false（永久有效）。
    fn token_expiring<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, bool> {
        Box::pin(async move {
            let record = match load_account(ctx, account).await {
                Ok(record) => record,
                // 读不到账号时不当成「临期」，避免后台每轮白跑
                Err(_) => return Ok(false),
            };
            let cred = match credentials::parse(&record.secret) {
                Ok(cred) => cred,
                Err(_) => return Ok(false),
            };
            if !cred.is_ak_sk() {
                return Ok(false);
            }
            Ok(credentials::token_expiring(&record.secret))
        })
    }

    // ── 目录 ──────────────────────────────────────────────────

    fn models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
    ) -> SendFuture<'a, proto::ModelList> {
        Box::pin(async move {
            // 读不到账号（还没加号）时给 V2 静态兜底 —— 「没有账号」是正常
            // 状态，不该让 /v1/models 变成红色失败。
            let Ok(record) = load_account(ctx, account).await else {
                return Ok(proto::ModelList {
                    models: models::static_v2(),
                    meta: proto::CatalogMeta::default(),
                });
            };
            match credentials::parse(&record.secret) {
                Ok(cred) => Ok(models::list_for(&cred)),
                Err(_) => Ok(proto::ModelList {
                    models: models::static_v2(),
                    meta: proto::CatalogMeta::default(),
                }),
            }
        })
    }

    fn refresh_models<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        account: &'a str,
        force: bool,
    ) -> SendFuture<'a, proto::RefreshOutcome> {
        Box::pin(async move {
            let record = match load_account(ctx, account).await {
                Ok(record) => record,
                // 一个账号都没有：`unchanged()` 而不是 `failed()`，与网关
                // 「没加号是正常状态」同口径（脚本 / CI 不该看到红色失败）。
                Err(_) if account.is_empty() => {
                    return Ok(proto::RefreshOutcome::unchanged());
                }
                Err(error) => return Ok(proto::RefreshOutcome::failed(error)),
            };
            let cred = match credentials::parse(&record.secret) {
                Ok(cred) => cred,
                Err(error) => return Ok(proto::RefreshOutcome::failed(error)),
            };
            let api_key = if cred.is_ak_sk() {
                String::new()
            } else {
                cred.api_key.clone()
            };
            // 账号级代理由网关在请求里给（`ConversationRequest.proxy`）；目录
            // 刷新这条 RPC 没有代理入参，走网关默认出口（`ctx.env.proxy`）。
            Ok(models::refresh(None, &ctx.env.proxy, &cred, &api_key, force).await)
        })
    }

    // ── 账号 ──────────────────────────────────────────────────

    /// 规范化一条账号：解析凭证 →（AK/SK 模式）真打一次 oauth 换取 access_token
    /// 作为探活，并把结果一起落库。
    ///
    /// ── 为什么 AK/SK 模式要真打一次 ────────────────────────────
    /// AK/SK 本身没有「格式」可校验（就是两串随机字符），唯一的判据是
    /// 上游认不认。oauth 端点便宜、且换到的 token 正是首次对话要用的，
    /// 一次调用同时完成「验证」与「预热」。
    ///
    /// ── 为什么 API Key 模式不强制探活 ──────────────────────────
    /// 唯一可用的探活端点是 `GET /v2/models`。探活失败若是**网络**原因
    /// （离线、代理没配好），拒绝落库会让用户连「先加进来、回头再修网络」
    /// 都做不到 —— 所以只在**上游明确拒绝**（401/403）时拒绝落库，
    /// 其余失败照常保存（错误留给第一次对话如实报出）。
    fn normalize_account<'a>(
        &'a self,
        ctx: &'a PluginCtx,
        input: proto::NormalizeInput,
    ) -> SendFuture<'a, proto::NormalizedAccount> {
        Box::pin(async move {
            let payload: Value = match &input.source {
                proto::NormalizeSource::Manual { payload } => payload.clone(),
                proto::NormalizeSource::FileText { content, .. } => {
                    let trimmed = content.trim();
                    match serde_json::from_str::<Value>(trimmed) {
                        Ok(parsed) => parsed,
                        // 纯文本文件：整段当 API Key
                        Err(_) => json!({ "apiKey": trimmed }),
                    }
                }
                proto::NormalizeSource::FileBase64 { .. } => {
                    return Err("文心不支持二进制文件导入，请粘贴千帆 API Key 或 AK/SK".to_string());
                }
                proto::NormalizeSource::Desktop => {
                    return Err(
                        "文心不支持导入桌面端登录态，请到百度智能云控制台复制 API Key 后粘贴"
                            .to_string(),
                    );
                }
            };
            let cred = credentials::parse(&payload)?;
            let display = input
                .name
                .clone()
                .or_else(|| {
                    payload
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| {
                    format!(
                        "文心 {} {}",
                        if cred.is_ak_sk() { "AK/SK" } else { "API Key" },
                        credentials::token_tail(&cred.api_key)
                    )
                });

            let mut secret = json!({
                "mode": cred.mode,
                "apiKey": cred.api_key,
            });
            if cred.is_ak_sk() {
                secret["secretKey"] = json!(cred.secret_key);
                if !cred.chat_base.is_empty() {
                    secret["baseUrl"] = json!(cred.chat_base);
                }
                let (token, expires_at) =
                    credentials::fetch_v1_token(input.proxy.as_ref(), &ctx.env.proxy, &cred).await?;
                secret["accessToken"] = json!(token);
                secret["expiresAt"] = json!(expires_at);
            } else {
                if let Some(base) = payload.get("baseUrl").and_then(Value::as_str) {
                    secret["baseUrl"] = json!(base.trim());
                }
                credentials::probe_api_key(input.proxy.as_ref(), &ctx.env.proxy, &cred).await?;
            }

            let identity = credentials::identity_of(&cred);
            let mut public = account_view_of(&secret);
            public["name"] = json!(display);
            Ok(proto::NormalizedAccount {
                secret,
                public,
                identity,
                desktop: false,
            })
        })
    }

    fn account_view(&self, secret: &Value) -> Result<proto::AccountView, String> {
        Ok(account_view_of(secret))
    }

    fn account_identity(&self, secret: &Value) -> Result<String, String> {
        let cred = credentials::parse(secret)?;
        Ok(credentials::identity_of(&cred))
    }
}

/// 公开形态（**绝不含凭证本体**，见模块头）。
fn account_view_of(secret: &Value) -> Value {
    let mode = secret
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or(credentials::MODE_API_KEY);
    let key = secret
        .get("apiKey")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut view = json!({
        "tokenTail": credentials::token_tail(key),
        "authMode": if mode == credentials::MODE_AK_SK { "AK/SK" } else { "API Key" },
    });
    if let Some(expires_at) = secret.get("expiresAt").and_then(Value::as_i64) {
        view["expiresAt"] = json!(expires_at);
    }
    view
}

/// 内容拦截的提示（与 ModelScope 同口径：拦截按逐字匹配，不是账号问题）。
const CONTENT_BLOCK_HINT: &str =
    "；内容被上游内容安全策略拒绝（`unsafe_request` 族，不是账号问题）：\
若为客户端 system 模板误报，可在设置页「通用 → 系统提示词」切到「替换」模式；\
若为对话内容本身触发审核，请调整后重试";

/// V1 旧端点的错误码 → 四档。
///
/// ── 依据的强弱要分清 ────────────────────────────────────────
///   · `110`（Access token invalid or no longer valid）**有官方原文**：
///     https://ai.baidu.com/ai-doc/WENXINWORKSHOP/jlil56u11 的「错误码」节。
///   · `4 / 17 / 18 / 19`（请求总量 / 日限额 / QPS / 累计上限）与
///     `13 / 14`（取 service token 失败 / IAM 认证失败）来自百度开放平台
///     错误码表的长期口径，本轮未抓到该页原文（页面 JS 渲染）。
///     **取舍方向是「宁可多换号，不要卡死」**：把限额误判成 Fatal 会让账号
///     永不轮换（每个请求都撞同一面墙）；反过来误判只会让账号冷却 10 分钟。
///   · 其余码一律 Fatal —— 未确认语义的码不该触发任何自动动作。
fn classify_v1(status: u16, code: i64, body: &Value) -> proto::ErrorClass {
    let raw = body
        .get("error_msg")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or("上游错误");
    let message = format!("上游返回 {status}: {raw}（error_code {code}）");
    match code {
        // access_token 失效 / 过期：唯一的自动恢复路径
        110 | 111 => proto::ErrorClass::TokenExpired { message },
        // 请求上限四兄弟：分钟级 / 日级窗口
        4 | 17 | 18 | 19 => proto::ErrorClass::QuotaLimited {
            reset_at: None,
            message: format!("文心旧版接口触发调用上限（{raw}），换账号或稍后重试"),
            upstream_code: Some(code),
            status,
        },
        // 取 token 失败 / IAM 认证失败：凭证链问题，刷新后可恢复
        13 | 14 => proto::ErrorClass::TokenExpired { message },
        // 无权限访问（数据 / 应用）：不是凭证过期，重刷无用
        6 | 15 => proto::ErrorClass::Fatal {
            status,
            message,
            upstream_code: Some(code),
        },
        // 参数错误
        100 => proto::ErrorClass::Fatal {
            status,
            message,
            upstream_code: Some(code),
        },
        _ => proto::ErrorClass::Fatal {
            status,
            message,
            upstream_code: Some(code),
        },
    }
}
