//! Free Pi（`api.freepi.ai`，免费 pi 编码代理）：OpenAI 兼容直通转发 +
//! `/client-version` 模型目录。账号层见 `account_store::freepi_accounts`
//! （账号 = 粘贴的 90 天登录 JWT）。
//!
//! ── 协议依据（全部来自官方 CLI 源码，本仓库 `free-pi-cli/` 检出）──
//!   · 端点：上游根 `https://api.freepi.ai`（`free-pi-cli/src/env.ts`
//!     `DEFAULT_BASE_URL`）；对话走 **`POST {根}/v1/chat/completions`**
//!     （`free-pi-cli/src/provider.ts::completionsBaseUrl`：pi 的
//!     openai-completions API 基于 openai SDK，会在 baseURL 后追加
//!     `/chat/completions`，所以 base 必须以 `/v1` 收尾，注释注明
//!     2026-08-14 实测缺 `/v1` 每发必 404）；
//!   · 鉴权：JWT 就是 apiKey（`provider.ts` `apiKey: jwt`）→ 标准
//!     `Authorization: Bearer <jwt>`，无签名 / HMAC / DPoP；
//!   · 伪装头：每次补全都带 `x-session-id`（逻辑会话 id，R19 并发会话闸门
//!     的判定键）与 `x-client-version`（服务端强制更新闸门读它做
//!     `minCliVersion` 比对，`provider.ts` #37 注释）。本网关的取值口径：
//!     `x-session-id` = **网关入站请求里的会话 id**（客户端带来什么就透什么，
//!     归一化与 `plugin_host::rpc::session_of` 逐字一致；绝不本地编造），
//!     `x-client-version` = 官方 CLI 当前版本常量；
//!   · 目录：`GET {根}/client-version`（**根路径，不带 `/v1`**——CLI 的
//!     `update-check.ts` 就这么调），响应 `models[]` 为
//!     `{id, name}`（`vendor/shared/api.ts::ClientVersionResponseSchema`），
//!     `id` 即请求体里要发的 model 名；另有单值 `model` 字段做旧服务端兼容
//!     （CLI 的回退链：`models[]` → `model`）；
//!   · 错误契约：`vendor/shared/errors.ts` 给出**code → HTTP 状态**的完整
//!     映射与 `ERROR_CODES` 全集；正文信封 `{code, message, limit?,
//!     retry_after_seconds?, clears_at?, remaining_credits_today?, lane?,
//!     hourly_ceiling?}`（`error-notice.ts::parseErrorEnvelope`，即
//!     `"<status>: {json}"` 里那段 JSON 的形状）；429 文案由
//!     `vendor/shared/credits.ts::formatLimitNotice` 生成（"daily limit, …" /
//!     "hourly limit, back in N min, …"）。**本家 classify_error 的每一条
//!     判据都取自这些实测契约，没有一条是猜的。**
//!
//! ── 与「实现搬进插件」的架构分流 ────────────────────────────────
//! 其余 21 家的转发与账号实现在提供商插件里，本仓库只留身份；Free Pi 是
//! `provider-adaptation` 拆家**之后**新增的内置家：上游就是标准 OpenAI
//! 兼容端点 + 静态 Bearer（见上），没有桌面登录态、没有续期、没有签名，
//! 网关内置的执行体只有「拼 URL / 头 / 体」这类纯计算，不持有任何
//! 「官方客户端登录态读取」路径 —— 账号层（`freepi_accounts.rs`）与账号
//! 注释里的约定一致：落库形态与将来插件接管时读的完全同形，插件到达后
//! `adapter_by_id` 插件优先，本适配器自然退为兜底。
//!
//! ── 能力位口径（每一条都有上游证据，不是「还没做」）─────────────
//!   · supports_chat = true：上游就是 chat/completions；
//!   · supports_model_refresh = true：`GET /client-version` 就是目录接口；
//!   · supports_refresh = **false**：CLI 鉴权面只有设备授权三步与 `/me`
//!     探活，没有任何 refresh 路由（`free-pi-cli/src/auth-flow.ts` 全文可查）；
//!     JWT 是 90 天凭证，过期唯一出路是重跑浏览器授权再贴一次 —— 依据见
//!     `account_store::freepi_accounts` 模块头（那里要求本文件落地时保持
//!     false 并引用它，就是这条）；
//!   · supports_usage = **true**：上游有独立的余额查询接口 —— `GET /me/stats`
//!     （CLI 的用量工具就打它，`free-pi-cli/src/usage-tool.ts::fetchUsageText`
//!     可查），实现见 `usage`；展示只有 percent / credits 两种形态
//!     （上游 founder decision：免费额度永不用美元展示，见 `usage` 模块头）。
//!
//! ── 硬约束 ──────────────────────────────────────────────────
//! release 是 `panic=abort`：本模块零 unwrap/expect/panic；目录缓存持锁
//! 期间不打日志（`catalog_cache` 的死锁约束）。

mod forward;
mod models;
mod usage;

use serde_json::Value;

use super::adapter::{ChatRequestPlan, ModelRefreshOutcome, ProviderAdapter, UpstreamErrorClass};
use super::ProviderKind;
use crate::server::core::account_store::AccountStore;
use crate::server::errors::GatewayError;

/// 本家 provider id（与注册表同源，见 `providers::mod` 的 `kind_id`）。
pub const ID: &str = super::kind_id(ProviderKind::FreePi);

/// 上游根（`free-pi-cli/src/env.ts::DEFAULT_BASE_URL`）。
pub const BASE_URL: &str = "https://api.freepi.ai";

/// 对话端点（`{BASE_URL}/v1/chat/completions`，依据见模块头）。
pub const CHAT_PATH: &str = "/v1/chat/completions";

/// 目录端点（根路径，**不带 `/v1`**；CLI `update-check.ts` 的调用形状）。
pub const CLIENT_VERSION_PATH: &str = "/client-version";

/// `x-client-version` 伪装头的取值：官方 CLI 的**当前发布版本**。
///
/// 服务端按它做 `minCliVersion` 强制更新闸门（低于下限回 426
/// `upgrade_required`，见 `vendor/shared/errors.ts`），所以这个值必须跟住
/// CLI 的发布版本，而不是编一个更新的号。
///
/// ── 取值的来源是「上游自己说的」，不是本仓库检出的 CLI 版本 ────────
/// 2026-11-12 实测：`GET https://api.freepi.ai/client-version` 回
/// `{"min":"0.6.19","latest":"0.6.19", …}`，与 npm `free-pi-cli@latest`
/// （0.6.19）一致。本仓库 `free-pi-cli/` 的检出是 **v0.4.10**（`package.json`
/// 的 `version`），已经低于上游 `min` —— 照抄它会稳定撞 426，所以取值改成
/// 上游报的 `latest`。**上游 `min` 涨上来时要同步改这里**（`/client-version`
/// 就是那个权威读数，不需要猜）。
pub const CLIENT_VERSION: &str = "0.6.19";

/// 静态实例（`adapter_for` 要 `&'static dyn`；本适配器无状态）。
pub static FREE_PI_ADAPTER: FreePiAdapter = FreePiAdapter;

/// 目录刷新元数据（remote 位 + 最后落地时刻）。
///
/// `catalog::refresh_meta` 对 Free Pi 取这里 —— 它是本模块对目录内部状态
/// （`models`）的**唯一**读出口，别处不要再开第二条缝。
pub(crate) fn catalog_meta() -> (bool, i64) {
    (models::remote_refreshed(), models::last_refreshed_at())
}

/// Free Pi 适配器。
pub struct FreePiAdapter;

impl ProviderAdapter for FreePiAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::FreePi
    }

    /// 目录：远程清单（`/client-version`，进程缓存 + 落盘缓存），
    /// 没拉到过 / 拉不到时回静态兜底（`deepseek/deepseek-v4-flash`）。
    fn list_models(&self) -> Vec<Value> {
        models::list()
    }

    /// `POST {BASE_URL}/v1/chat/completions`（OpenAI 兼容透传，见
    /// `forward::build_chat_request`）。
    fn build_chat_request(
        &self,
        account: &Value,
        body: &Value,
        client_headers: &axum::http::HeaderMap,
    ) -> Result<ChatRequestPlan, GatewayError> {
        forward::build_chat_request(account, body, client_headers)
    }

    /// 错误分类：四档口径与判据来源见 `forward::classify_error`。
    fn classify_error(&self, status: u16, error_body: &Value) -> UpstreamErrorClass {
        forward::classify_error(status, error_body)
    }

    /// 有远程目录（`GET /client-version`）
    fn supports_model_refresh(&self) -> bool {
        true
    }

    /// 刷模型目录：用指定账号（空串 = 该家队首可用账号）的 JWT 打
    /// `/client-version`。判据与文案见 `models::refresh`。
    fn refresh_models<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
        force: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ModelRefreshOutcome> + Send + 'a>>
    {
        Box::pin(async move { models::refresh(store, account_id, force).await })
    }

    /// 取可用访问令牌（90 天 JWT；无续期，过期由 401 → needsRelogin 那条
    /// 既有链路兜底，见 `forward::classify_error` 的 401 分支说明）。
    fn ensure_access_token<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>>
    {
        Box::pin(async move {
            let session = models::account_session(store, account_id).ok_or_else(|| {
                GatewayError::with_status(401, "Free Pi 账号不存在、未启用或没有凭证")
            })?;
            let token = session
                .session
                .get("auth")
                .and_then(|auth| auth.get("accessToken"))
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("")
                .to_string();
            if token.is_empty() {
                return Err(GatewayError::with_status(
                    401,
                    "Free Pi 账号缺少登录令牌，请重新粘贴 JWT",
                ));
            }
            Ok(token)
        })
    }

    /// 无续期接口（依据见模块头与 `freepi_accounts` 模块头的约定）。
    fn supports_refresh(&self) -> bool {
        false
    }

    /// **显式失败**，不要退回 trait 的默认实现。
    ///
    /// ── 为什么必须覆写 ────────────────────────────────────────
    /// 默认实现是「返回 `ensure_access_token`」，也就是把这枚**没有变过的**
    /// JWT 原样交回去、还报成功。`provider_loop` 的 401 动作 2（token 失效 →
    /// 刷新后同一账号重试一次）只看返回 `Ok` 与否，于是它会把同一枚已被上游
    /// 拒收的 JWT 再发一次 —— 必然又是 401，白打一次上游（这家上游对 429 /
    /// 并发会话很敏感，翻倍请求量是实打实的代价），然后才落动作 2b 标记
    /// `needsRelogin`。
    ///
    /// 返回 `Err` 让动作 2 立刻收手，401 直接进动作 2b：账号落
    /// `needsRelogin`、错误原样透传给客户端 —— 正是「90 天重贴」那条流
    /// （文案与 `api::accounts::refresh_account` 的 400 指路同源）。
    /// 这条 Err 只被编排层当布尔用，不会成为用户可见文案。
    fn refresh_access_token<'a>(
        &'a self,
        _store: &'a AccountStore,
        _account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, GatewayError>> + Send + 'a>>
    {
        Box::pin(async move {
            Err(GatewayError::with_status(
                401,
                "Free Pi 的登录令牌没有刷新机制（上游没有续期接口，令牌 90 天有效）：\
                 请重新完成浏览器登录，然后重新粘贴新令牌",
            ))
        })
    }

    /// 有余额查询：上游的 `GET /me/stats`（依据见模块头）。
    fn supports_usage(&self) -> bool {
        true
    }

    /// 余额 / 用量查询：`GET /me/stats` + percent / credits 展示口径
    /// （实现与字段映射见 `usage::query_usage`）。
    fn query_usage<'a>(
        &'a self,
        store: &'a AccountStore,
        account_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, GatewayError>> + Send + 'a>>
    {
        Box::pin(async move { usage::query_usage(store, account_id).await })
    }

    fn supports_chat(&self) -> bool {
        true
    }
}
