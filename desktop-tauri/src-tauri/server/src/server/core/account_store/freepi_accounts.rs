//! free-pi（免费 pi 编码代理）账号：**账号 = 粘贴的登录 JWT**。
//!
//! ── 这家的凭证形态（依据）────────────────────────────────────
//! free-pi 上游走 GitHub 设备授权换一枚 JWT（`free-pi-cli/src/auth-flow.ts`：
//! `/auth/github/device` → 浏览器授权 → 轮询 `/auth/token`），鉴权就是静态
//! `Authorization: Bearer <jwt>` 打 OpenAI 兼容端点。设备授权**必须开浏览器**，
//! 网关代登录做不了，所以面板走**粘贴 JWT 表单**：账号就是用户粘贴的那枚
//! 长期令牌本身，身份取 JWT 载荷的 `sub`（`free-pi-cli/src/gravity-pixel.ts`
//! 明说 `sub` = 用户 id）。
//!
//! ── 为什么 supports_refresh = false（本家没有续期）───────────
//! 上游**没有令牌续期接口**：CLI 的鉴权面只有设备授权三步与 `/me` 探活，
//! 没有任何 refresh 路由（`auth-flow.ts` 全文可查）。JWT 本身是 **90 天**
//! 有效凭证（`free-pi-cli/src/provider.ts` R19 注释：「the JWT is a 90-day
//! credential」）。到期后唯一的续命方式是重跑一次浏览器设备授权拿到新 JWT、
//! 再到本页重新粘贴 —— 即「90 天重贴」流。因此本家：
//!   * 不写 `refreshToken`（上游根本不发）；
//!   * 刷新分派显式报 400 并指路重贴（`api::accounts::refresh_account` 的
//!     free-pi 分支，与 CatPaw / MonkeyCode 的「没有刷新机制」同款处理）；
//!   * 对应 `ProviderAdapter::supports_refresh` 的口径 = false（上游没有
//!     那个接口，不是「还没做」，见 provider-adaptation SKILL §2.4）。
//! 适配器本体尚未接线（ProviderKind / adapter 由身份与转发步骤接入），
//! 那边的 `supports_refresh()` 落地时必须保持 false 并引用本注释。
//!
//! ── 记录形状 ───────────────────────────────────────────────
//! ```json
//! {
//!   "id": "free-pi-account-8f2c1d4a6b90", // free-pi-account- + 12 位 hex（uid 派生）
//!   "provider": "free-pi",
//!   "name": "free-pi 账号 abcd",
//!   "uid": "987654321",                   // JWT sub（缺了不写键）
//!   "accessToken": "eyJ…",                // 凭证本体（只进存储，绝不下发）
//!   "tokenTail": "abcd",                  // 界面尾号
//!   "expiresAt": 1790000000000,           // JWT exp（秒 → 毫秒；缺了不写键）
//!   "source": "manual",
//!   "priority": 5, "enabled": true,
//!   "addedAt": 1730000000000, "updatedAt": 1730000000000
//! }
//! ```
//!
//! ── 公开形态红线 ───────────────────────────────────────────
//! JWT 本体**绝不进公开形态**（`to_freepi_public_account` 逐字段白名单，
//! 不做「整条记录减几个键」的写法）：只给 `tokenTail` 与 `hasToken`
//! 存在标记，到期时刻从记录 `expiresAt` 透出（由公开层统一派生各到期键）。
//! 日志同理：只记账号名与尾号，绝不记令牌。
//!
//! 幂等口径：同一家、同一 `sub` 重复粘贴 = 更新既有记录（90 天内重登换新
//! JWT 也落在这条：账号还是那个人）；读不出 `sub` 时退化为按令牌原文匹配。
//!
//! ── 硬约束 ─────────────────────────────────────────────────
//! 本文件全是「读-改-写」文件操作，**没有任何网络请求**（持锁不做网络）。
//! 绝不 unwrap/expect（release 是 panic=abort）。

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use super::priority::next_free_priority;
use super::sql;
use super::state::{mark_name_custom, StoredAccount};
use super::store::{AccountStore, AccountStoreError};
use super::store_util::{token_tail_of, truncate_chars};
use super::{FREEPI_PROVIDER_ID, MAX_TOKEN_LENGTH};
use crate::server::logging;

/// 备注名长度上限（与另外几家一致）
const MAX_NAME_CHARS: usize = 100;

/// `add_freepi_account` 的返回：落盘记录的公开形态 + 这次是新建还是更新。
///
/// 与 `CustomAccountUpsert` 同一形状的理由：API 层把 `created` 原样透传，
/// 前端才能把提示说成「已更新」（90 天内重贴是正常操作，照搬「已添加」
/// 会让人以为列表会多出一条）。
#[derive(Debug)]
pub struct FreepiAccountUpsert {
    /// 落盘记录的公开形态（与列表快照同一形状，见 `public_account`）
    pub account: Value,
    /// `true` = 新建了一条记录；`false` = 合并更新了既有记录（同 uid 重贴）
    pub created: bool,
}

/// 从粘贴 payload 里取令牌：认 `token` / `accessToken` / `jwt` 三个键
/// （面板表单用 `token`，其余两个是手改请求与导入的容错），去掉可选的
/// `Bearer ` 前缀与首尾空白。取不到非空串报 400。
fn pasted_jwt(payload: &Value) -> Result<String, String> {
    let Some(object) = payload.as_object() else {
        return Err("账号内容必须是 JSON 对象".to_string());
    };
    let raw = ["token", "accessToken", "jwt"]
        .iter()
        .find_map(|key| object.get(*key).and_then(Value::as_str))
        .unwrap_or("");
    let token = raw
        .trim()
        .strip_prefix("Bearer ")
        .or_else(|| raw.trim().strip_prefix("bearer "))
        .unwrap_or(raw.trim())
        .trim();
    if token.is_empty() {
        return Err("请粘贴 free-pi 的登录令牌（JWT）".to_string());
    }
    if token.chars().count() > MAX_TOKEN_LENGTH {
        return Err("令牌过长：请粘贴 free-pi 的登录令牌本身，不要整段文档一起贴进来".to_string());
    }
    Ok(token.to_string())
}

/// base64url（无填充）解码 —— JWT 载荷段的形态（与 `chatglm_accounts` 同款）。
fn base64_url_decode(text: &str) -> Option<Vec<u8>> {
    let mut buffer: Vec<u8> = Vec::with_capacity(text.len());
    let mut accumulator: u32 = 0;
    let mut bits: u32 = 0;
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            b'=' => continue,
            _ => return None,
        };
        accumulator = (accumulator << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            buffer.push(((accumulator >> bits) & 0xff) as u8);
        }
    }
    Some(buffer)
}

/// 解 JWT 载荷（**不解签**，只读声明 —— 与 CLI 的 `userIdFromJwt` 同一口径）。
/// 三段式 + 载荷是 JSON 对象才算 JWT；否则 None。
fn jwt_payload(token: &str) -> Option<Value> {
    let mut segments = token.split('.');
    let header = segments.next()?;
    let payload = segments.next()?;
    let signature = segments.next()?;
    if header.is_empty() || payload.is_empty() || signature.is_empty() || segments.next().is_some() {
        return None;
    }
    let bytes = base64_url_decode(payload)?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    value.is_object().then_some(value)
}

/// 载荷里的字符串 / 数字型声明（`sub` 这类），数字原样转字符串。
fn claim_text(claims: &Value, key: &str) -> Option<String> {
    match claims.get(key) {
        Some(Value::String(text)) => {
            let text = text.trim();
            (!text.is_empty()).then(|| text.to_string())
        }
        Some(value @ Value::Number(_)) => Some(value.to_string()),
        _ => None,
    }
}

/// 载荷里的 `exp`（秒）→ 毫秒时刻；缺了 / 非正数返回 None。
fn claim_exp_millis(claims: &Value) -> Option<i64> {
    let seconds = claims.get("exp").and_then(Value::as_i64)?;
    (seconds > 0).then(|| seconds.saturating_mul(1000))
}

/// 本家账号的稳定 id：`freepi-account-` + sha256(provider \0 seed) 前 12 位。
///
/// seed 优先用 `sub`（同一个人重贴 → 同一条记录），读不出 `sub` 退化成令牌
/// 原文（同令牌合并、不同令牌各自成条）。带 provider 前缀做域隔离：
/// 主键全局唯一，`user-<uid>` 那种裸形态会与别家撞号（custom / 插件身份
/// 同一条纪律）。
fn freepi_new_id(seed: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(FREEPI_PROVIDER_ID.as_bytes());
    hash.update([0]);
    hash.update(seed.as_bytes());
    let suffix: String = format!("{:x}", hash.finalize()).chars().take(12).collect();
    format!("{FREEPI_PROVIDER_ID}-account-{suffix}")
}

impl AccountStore {
    /// 添加 / 更新一个 free-pi 账号（`POST /api/accounts` 的 free-pi 分支）。
    ///
    /// `payload` 只认令牌键（见 [`pasted_jwt`]）与 `name`（由调用方从请求体
    /// 取好传入），其余键忽略 —— 与各家「未知键不动」口径一致。
    pub fn add_freepi_account(
        &self,
        payload: &Value,
        name: Option<&str>,
    ) -> Result<FreepiAccountUpsert, AccountStoreError> {
        let token = pasted_jwt(payload).map_err(AccountStoreError::bad_request)?;
        let claims = jwt_payload(&token).ok_or_else(|| {
            AccountStoreError::bad_request(
                "粘贴的内容不是 free-pi 登录令牌：应为三段式 JWT（在 free-pi 登录后从凭据文件或登录输出复制）",
            )
        })?;
        let uid = claim_text(&claims, "sub").unwrap_or_default();
        let expires_at = claim_exp_millis(&claims);
        let tail = token_tail_of(&token);

        let _guard = self.guard();
        let existing = self.with_conn(&_guard, |conn| {
            let records = sql::load_by_provider(conn, FREEPI_PROVIDER_ID)?;
            Ok(records.into_iter().find(|record| {
                (!uid.is_empty() && record.uid() == uid)
                    || record.access_token() == token
            }))
        })?;
        let id = match existing.as_ref() {
            Some(record) => record.id().to_string(),
            None => {
                let seed = if uid.is_empty() { token.as_str() } else { uid.as_str() };
                let id = freepi_new_id(seed);
                if self.with_conn(&_guard, |conn| sql::load_by_id(conn, &id))?.is_some() {
                    return Err(AccountStoreError::new(
                        format!("账号 id「{id}」已被占用，请重试或检查已有记录"),
                        409,
                    ));
                }
                id
            }
        };

        // 备注名兜底链：显式传入 → 记录里原有的 → 「free-pi 账号 + 尾号」
        // （与 custom 的兜底同一取向：尾号回答「哪枚令牌」）。
        let explicit_name = name
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| truncate_chars(value, MAX_NAME_CHARS));
        let explicit = explicit_name.is_some();
        let record_name = explicit_name
            .or_else(|| {
                existing
                    .as_ref()
                    .map(StoredAccount::name)
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| {
                if tail.is_empty() {
                    "free-pi 账号".to_string()
                } else {
                    format!("free-pi 账号 {tail}")
                }
            });

        let priority = existing
            .as_ref()
            .map(StoredAccount::priority)
            .unwrap_or_else(|| {
                let used = self
                    .with_conn(&_guard, |conn| sql::priorities_all(conn))
                    .unwrap_or_default();
                next_free_priority(&used)
            });

        let now = logging::now_ms();
        // 从既有记录的字段表出发（不变量「未知字段全量保留」），新建时是空表
        let mut fields: Map<String, Value> = existing
            .as_ref()
            .map(|item| item.fields().clone())
            .unwrap_or_default();
        fields.insert("id".to_string(), Value::String(id));
        fields.insert(
            "provider".to_string(),
            Value::String(FREEPI_PROVIDER_ID.to_string()),
        );
        fields.insert("name".to_string(), Value::String(record_name.clone()));
        mark_name_custom(&mut fields, explicit, existing.as_ref());
        fields.insert("accessToken".to_string(), Value::String(token));
        fields.insert("tokenTail".to_string(), Value::String(tail.clone()));
        // uid / expiresAt：这次有值就写，没有就摘键（空串与 0 不是有效身份 /
        // 有效期，落盘只会让读侧把「缺失」误读成「空值」）
        match (uid.is_empty(), expires_at) {
            (false, _) => fields.insert("uid".to_string(), Value::String(uid)),
            (true, _) => fields.remove("uid"),
        };
        match expires_at {
            Some(millis) => fields.insert("expiresAt".to_string(), Value::from(millis)),
            None => fields.remove("expiresAt"),
        };
        fields.insert(
            "source".to_string(),
            Value::String(
                existing
                    .as_ref()
                    .map(StoredAccount::source)
                    .filter(|value| !value.is_empty())
                    .unwrap_or_else(|| "manual".to_string()),
            ),
        );
        fields.insert("priority".to_string(), Value::from(priority));
        fields.insert(
            "enabled".to_string(),
            Value::Bool(existing.as_ref().map(StoredAccount::enabled).unwrap_or(true)),
        );
        fields.insert(
            "addedAt".to_string(),
            Value::from(
                existing
                    .as_ref()
                    .map(StoredAccount::added_at)
                    .filter(|value| *value != 0)
                    .unwrap_or(now),
            ),
        );
        fields.insert("updatedAt".to_string(), Value::from(now));

        let saved = StoredAccount::from_map(fields);
        self.with_conn(&_guard, |conn| sql::put(conn, &saved))?;
        // 日志只记名字与尾号：JWT 本体绝不进日志（公开形态红线，见模块头）
        logging::log(
            "[Accounts]",
            &format!(
                "{} free-pi 账号{}: {}（尾号 {tail}）",
                if existing.is_some() { "🔄" } else { "✅" },
                if existing.is_some() { "已更新" } else { "已添加" },
                record_name,
            ),
        );
        Ok(FreepiAccountUpsert {
            account: self.public_account(&saved),
            created: existing.is_none(),
        })
    }

    /// free-pi 账号的**公开形态**（进 HTTP 响应）。
    ///
    /// 逐字段白名单：JWT 本体（`accessToken`）不在这里的任何一条 insert 里，
    /// 只给尾号与「有没有令牌」的存在标记；到期时刻从记录 `expiresAt` 透出，
    /// 各到期键（`accessTokenExpiresAt` 等）由 [`AccountStore::public_account`]
    /// 的统一层派生。`hasToken` 恒给布尔：界面据此显示「已配置 / 需重贴」。
    pub fn to_freepi_public_account(&self, record: &StoredAccount) -> Value {
        let has_token = record.has_token();
        let mut public = Map::new();
        for (key, value) in [
            ("id", Value::String(record.id().to_string())),
            ("provider", Value::String(record.provider())),
            ("name", Value::String(record.name())),
            ("uid", record.get("uid").cloned().unwrap_or(Value::Null)),
            ("tokenTail", record.get("tokenTail").cloned().unwrap_or(Value::String(String::new()))),
            ("hasToken", Value::Bool(has_token)),
            (
                "expiresAt",
                record.get("expiresAt").cloned().unwrap_or(Value::Null),
            ),
            ("source", Value::String(record.source())),
            ("priority", Value::from(record.priority())),
            ("enabled", Value::Bool(record.enabled())),
            ("addedAt", Value::from(record.added_at())),
            ("updatedAt", Value::from(record.updated_at())),
            (
                "proxy",
                crate::server::core::proxies::describe_account_proxy(Some(&record.proxy())),
            ),
            (
                "rateLimits",
                record
                    .get("rateLimits")
                    .cloned()
                    .unwrap_or_else(|| json!({})),
            ),
            // 「可用」= 已启用且有凭证（与各家同一语义）
            ("available", Value::Bool(record.enabled() && record.has_credentials())),
        ] {
            public.insert(key.to_string(), value);
        }
        Value::Object(public)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use serde_json::json;

    /// 测试专用的 base64url（无填充）编码 —— 与 [`base64_url_decode`] 对偶。
    fn b64url(text: &str) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(text.as_bytes())
    }

    /// 拼一枚三段式 JWT（不解签的假签名即可，本家只读载荷）。
    fn fixture_jwt(sub: Option<&str>, exp: Option<i64>) -> String {
        let mut claims = Map::new();
        if let Some(sub) = sub {
            claims.insert("sub".to_string(), json!(sub));
        }
        if let Some(exp) = exp {
            claims.insert("exp".to_string(), json!(exp));
        }
        format!(
            "{}.{}.sig",
            b64url(r#"{"alg":"HS256","typ":"JWT"}"#),
            b64url(&Value::Object(claims).to_string()),
        )
    }

    /// 独立临时库：标签必须每个用例唯一 —— `TempDb` 的路径由
    /// 「标签 + 进程号」决定，而测试并行跑在同一进程里，重名会互踩同一个
    /// SQLite 文件（实测 `locking protocol`）。
    fn temp_store(label: &str) -> (AccountStore, crate::server::db::test_temp::TempDb) {
        let (db, guard) = crate::server::db::test_temp::TempDb::open(label);
        (AccountStore::with_db(Some(db)), guard)
    }

    /// JWT 载荷解析：`sub` / `exp` 都读得到，非 JWT 原文被拒。
    #[test]
    fn jwt_payload_extracts_claims_and_rejects_non_jwt() {
        let token = fixture_jwt(Some("987654321"), Some(1_790_000_000));
        let claims = jwt_payload(&token).expect("三段式 JWT 应当解得出载荷");
        assert_eq!(claim_text(&claims, "sub").as_deref(), Some("987654321"));
        assert_eq!(claim_exp_millis(&claims), Some(1_790_000_000_000));

        assert!(jwt_payload("not-a-jwt").is_none(), "非 JWT 不能被当载荷");
        assert!(jwt_payload("a.b").is_none(), "两段式不算 JWT");
        assert!(
            jwt_payload(&fixture_jwt(None, None)).is_some(),
            "没有 sub/exp 的三段式 JWT 仍应解得出（声明可缺）"
        );
    }

    /// 粘贴容错：`Bearer ` 前缀与首尾空白剥掉；三个键名都认；空值报错。
    #[test]
    fn pasted_token_is_trimmed_and_bearer_prefix_stripped() {
        let token = fixture_jwt(Some("u1"), None);
        let with_bearer = format!("  Bearer {token}  ");
        assert_eq!(
            pasted_jwt(&json!({ "token": with_bearer })).unwrap(),
            token,
            "Bearer 前缀与空白应当剥掉"
        );
        assert_eq!(
            pasted_jwt(&json!({ "accessToken": token })).unwrap(),
            token,
            "accessToken 键也要认（手改请求的容错）"
        );
        assert!(pasted_jwt(&json!({})).is_err(), "缺令牌必须报错");
        assert!(pasted_jwt(&json!({ "token": "   " })).is_err(), "空白令牌必须报错");
    }

    /// **添加 → 列表往返，公开形态只见尾号与存在标记**：JWT 本体不得出现在
    /// 单条公开形态、列表快照的任何一处（红线条目，见模块头）。
    #[test]
    fn add_and_list_roundtrip_redacts_jwt_body() {
        let (store, _db) = temp_store("freepi-roundtrip");
        let token = fixture_jwt(Some("987654321"), Some(1_790_000_000));
        let tail: String = token.chars().rev().take(4).collect::<Vec<_>>().into_iter().rev().collect();

        let upsert = store
            .add_freepi_account(&json!({ "token": token }), None)
            .expect("粘贴 JWT 应当能落库");
        assert!(upsert.created, "首次添加应当是新建");
        let public = &upsert.account;
        assert_eq!(
            public.get("tokenTail").and_then(Value::as_str),
            Some(tail.as_str()),
            "公开形态给尾号"
        );
        assert_eq!(public.get("hasToken"), Some(&Value::Bool(true)), "存在标记恒给");
        assert_eq!(public.get("uid").and_then(Value::as_str), Some("987654321"));
        assert_eq!(public.get("expiresAt"), Some(&Value::from(1_790_000_000_000_i64)));
        assert!(public.get("id").is_some() && public.get("provider").is_some());
        // 红线：整份公开形态的序列化里不允许出现令牌本体
        let serialized = public.to_string();
        assert!(
            !serialized.contains(&token),
            "公开形态绝不能携带 JWT 本体（泄漏 = 账号列表接口把凭证发给前端）"
        );
        // 列表快照同样过一遍红线 + 往返可见
        let list = store.list_visible_accounts();
        let accounts = list.get("accounts").and_then(Value::as_array).expect("快照应带 accounts");
        assert_eq!(accounts.len(), 1, "添加后列表应当有一条 free-pi 账号");
        assert!(
            !list.to_string().contains(&token),
            "列表快照绝不能携带 JWT 本体"
        );
        assert_eq!(
            accounts[0].get("tokenTail").and_then(Value::as_str),
            Some(tail.as_str()),
            "列表里的 free-pi 账号走本家公开形态（不是 workbuddy 兜底形状）"
        );
    }

    /// 同一 `sub` 重贴 = 更新同一条记录（90 天重贴流）；不同 `sub` 各自成条。
    #[test]
    fn repaste_same_sub_updates_same_record() {
        let (store, _db) = temp_store("freepi-repaste");
        let first = fixture_jwt(Some("42"), Some(1_700_000_000));
        let second = fixture_jwt(Some("42"), Some(1_790_000_000));

        let added = store.add_freepi_account(&json!({ "token": first }), None).unwrap();
        assert!(added.created);
        let repasted = store
            .add_freepi_account(&json!({ "token": second }), Some("我的 free-pi"))
            .unwrap();
        assert!(!repasted.created, "同 uid 重贴应当合并进既有记录");
        assert_eq!(
            repasted.account.get("id").and_then(Value::as_str),
            added.account.get("id").and_then(Value::as_str),
            "重贴不换主键"
        );
        assert_eq!(
            repasted.account.get("name").and_then(Value::as_str),
            Some("我的 free-pi"),
            "显式备注名生效"
        );

        let other = store
            .add_freepi_account(&json!({ "token": fixture_jwt(Some("43"), None) }), None)
            .unwrap();
        assert!(other.created, "不同 uid 应当新建一条");
        assert_ne!(
            other.account.get("id").and_then(Value::as_str),
            added.account.get("id").and_then(Value::as_str),
            "不同账号不能共用主键"
        );
        let list = store.list_visible_accounts();
        let accounts = list.get("accounts").and_then(Value::as_array).unwrap();
        assert_eq!(accounts.len(), 2, "两条 uid 不同的账号并存");
    }

    /// 非 JWT 粘贴必须 400 拒绝：落一条没有凭证语义的记录只会变成
    /// 「账号在列表里、转发必然失败」的静默坏账。
    #[test]
    fn rejects_non_jwt_paste() {
        let (store, _db) = temp_store("freepi-reject");
        let error = store
            .add_freepi_account(&json!({ "token": "hello world" }), None)
            .expect_err("非 JWT 应当被拒");
        assert_eq!(error.status_code, 400);
        let accounts = store
            .list_visible_accounts()
            .get("accounts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        assert!(accounts.is_empty(), "被拒的粘贴不能落库");
    }
}
