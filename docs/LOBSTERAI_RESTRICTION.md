# LobsterAI「账号暂时受限」根因分析

> 结论先行：这不是账号本身的问题，也不是额度用完。上游在**授权码换令牌的那一刻**
> 就给账号打上了 `TEMP_RESTRICTED`（24 小时），原因是请求被判定成「第三方转接」。
> 判定依据不是聊天内容或请求量 —— 该账号一条聊天请求都没有发过；最可能是
> **换码 / 首个请求的客户端指纹**（User-Agent 与缺失的客户端头）。

## 1. 现场证据

`GET /api/user/profile-summary` 的原始响应里带一段 `accountAccess`（插件把它
原样留在 `usage.raw` 里，界面此前只读积分、没读这一段）：

```json
{
  "accountAccess": {
    "state": "TEMP_RESTRICTED",
    "restrictedUntil": "2026-11-01T18:35:56.572Z",
    "serverTime": "2026-11-01T01:10:49.368Z",
    "message": "账号暂时无法使用相关服务，预计于 2026-11-02 02:35:56 (UTC+08:00) 恢复。请通过有道龙虾官方客户端或官网使用服务，勿通过第三方反向代理或插件转接、共享本账号的服务额度。再次出现异常使用可能导致永久停用。",
    "messageEn": "Your account is temporarily restricted until 2026-11-02 02:35:56 (UTC+08:00). Please use the official LobsterAI app or website. Do not relay or share your account's service credits through third-party proxies or plugins. Further abnormal use may result in permanent suspension."
  },
  "creditItems": [
    { "creditsRemaining": 300, "expiresAt": "2026-11-15T02:35:43", "label": "新用户赠送", "type": "free" }
  ]
}
```

时间线（均为 UTC+08:00）：

| 时刻 | 事件 |
| --- | --- |
| 02:35:43 | 账号注册 / 新用户赠送生效（`creditItems[0].expiresAt` 往前推 14 天） |
| 02:35:56 | 网关完成授权码换令牌并落库（`addedAt = 1793471756620`） |
| **02:35:56** | **`restrictedUntil` 恰好 = 落库时刻 + 24 小时** |
| 02:36:01 | 第一次 `profile-summary` 已经带着 `TEMP_RESTRICTED` 返回（积分仍是 300） |
| 02:36 起 | 网关每约 60 秒继续查一次余额（默认 `usageQuery.interval = 60` 秒） |

三个关键点：

1. **限制时刻 = 换码时刻 + 24h**，不是「用了一阵之后才被封」。说明判定发生在
   授权码交换（或紧随其后的第一个请求），与聊天量、并发、内容无关。
2. **积分一分没少（300）**，`state` 是 `TEMP_RESTRICTED` 而不是余额/配额类错误。
3. **该账号没有任何聊天请求**（`requests` 表里 `provider='lobsterai'` 为 0 行），
   所以「请求太多」不是原因。

## 2. 官方客户端与插件请求的差异（反编译 `app.asar`，2026.10.16）

从 `https://lobsterai.youdao.com/download-list` 取 macOS 安装包，解开
`LobsterAI.app/Contents/Resources/app.asar` 后读 `dist-electron/main.js`：

| 项目 | 官方客户端 | 插件（`agent2api-providers` beta.6） |
| --- | --- | --- |
| `X-LobsterAI-Client-Capabilities` | `virtual-models-v1,kimi-k3-agentic-v1,thinking-level-control-v1` | 只有后两项（**缺 `virtual-models-v1`**） |
| `X-LobsterAI-Client-Version` | 真实版本（安装包 `2026.10.16`） | 目录/聊天有；**余额与签到请求完全不发** |
| `User-Agent` | Electron `net.fetch` 默认 UA（真实客户端形态） | 写死 `LobsterAI/0.1.0`（**第三方参考实现的 UA，不是官方版本号**） |
| LLM 请求附加头 | `traceparent`、`x-lobster-trace-id`、用量归因头（`getLlmUsageHeaders`） | 全部没有 |
| 余额查询时机 | 启动预热一次（`runStartupCacheWarmup`，带能力/版本头） | 每 60 秒一次，只有 Authorization / Accept / UA |
| 换码请求 | `Content-Type` + `{authCode,keyfrom…}`（Electron 默认 UA） | 同左，但 UA 是 `LobsterAI/0.1.0` |

官方客户端的换码请求本身不带自定义头（`net.fetch` + `Content-Type`），所以
**最可疑的是 UA**：`LobsterAI/0.1.0` 既不是浏览器 UA，也不是真实客户端版本
（真实版本是日期式 `2026.10.16`）。这正好是第三方参考实现 `headers.go` 里
写死的那个值 —— 插件把它照抄了过来，等于主动穿上了「第三方客户端」的指纹。

其次是**余额轮询**：官方客户端只在启动时预热一次，插件默认 60 秒一次、
且这条请求连能力/版本头都不带。即便换码没被拦，这种形态也很容易在后续被
风控记为「插件中转」。上游警告原文里的「再次出现异常使用可能导致永久停用」
说明持续轮询是有实际风险的。

> 说明：以上是**相关性证据**（时间戳 + 客户端代码对比），不是上游判定逻辑的
> 源码。上游没有公开风控规则，无法 100% 断言是哪一条命中；但「换码即受限 +
> 零聊天请求 + 积分未动」把范围压到了客户端指纹这一类，而不是用量。

## 3. 本仓库做的处理

- `core/usage_query.rs`：识别 `raw.accountAccess.state != ACTIVE` 且
  `restrictedUntil` 在未来 → 本次查询记为失败（`code = provider_restricted`，
  `error` 用上游原文），并在**内存里**把该账号的自动查询挂起到恢复时刻之后。
  这样界面会如实显示「账号暂时受限…」，而不是「可用 300 积分」；也不会在
  限制期内每 60 秒继续打上游。
- 没有做任何绕过：人机验证、风控、共享额度都没有尝试规避。恢复只能等窗口
  结束或用官方客户端。

## 4. 给插件侧的建议（未在本仓库强改）

1. `USER_AGENT` 改成真实客户端形态（例如安装包版本 `LobsterAI/2026.10.16`），
   不要用参考实现的 `0.1.0`；
2. `CLIENT_CAPABILITIES` 补上 `virtual-models-v1`；
3. 余额 / 签到等控制面请求也带 `X-LobsterAI-Client-Version` 与能力头；
4. LobsterAI 的默认余额查询间隔从 60 秒调到 ≥ 30 分钟（或只手动查询）；
5. LLM 请求补 `traceparent` / `x-lobster-trace-id` 与用量归因头（若上游按
   归因缺失判第三方）。
