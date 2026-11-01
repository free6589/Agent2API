# 网页版账号（web 账号）支持说明

「网页版账号」指用**本人在 AI 官网的登录态**（Cookie / 网页令牌）接进网关，
而不是申请 API Key。它们和普通账号走同一条 `/v1/chat/completions`，区别只在
凭证来源和上游协议。

## 1. 现在支持哪些

| provider id | 界面名称 | 来源 | 凭证 |
| --- | --- | --- | --- |
| `chatglm` | 智谱清言 | 官方 `agent2api-web-providers` 包 | `chatglm_token`（或整段 Cookie） |
| `deepseek-web` | DeepSeek 网页 | 官方 `agent2api-web-providers` 包（beta.6 起） | `userToken`（localStorage）+ 可选 Cookie |
| `qwen-web` | 通义千问 网页 | 本仓库扩展包 `agent2api-web-extra`（`contrib/web-extra`） | `token`（localStorage 的 JWT）+ 可选 Cookie |

三家都归在「网页」分类下（插件 `pack.json` 的 `provider_category: "web"`），
添加账号页的「网页」分组里能看到。

## 2. 之前为什么 DeepSeek 网页显示「无法读取登录方式」

界面添加账号时先问 `GET /api/session/login/methods?provider=…`。这个接口要求
插件实现管理动作 `auth.methods`；网页插件只申报了**静态** `login_methods`
（`ProviderInfo.login_methods`），没有实现动态动作，于是接口回 501：

```
该提供商尚未申报动态登录方式，请使用既有登录入口
```

而桌面壳的 `gateway.rs::unwrap_envelope()` 会把 HTTP 错误压成纯字符串，
状态码在 UI 侧丢了，前端没法识别「501 = 没实现」这条正常分支，直接把
错误弹给用户 —— 于是「DeepSeek 网页」既没有表单也没有入口。

修复分三层（都在本仓库）：

1. **核心**：`plugin_host::login_methods_of()` 把插件静态申报的
   `login_methods` 塞进 `/api/session` 的 provider 摘要（`loginMethods`），
   界面不必再问动态接口就能画出表单；
2. **界面**：`add-account-bridge.ts` 认「尚未申报动态登录方式 / 501」这类
   文案，走 `legacyMethods()` 回退；`add-account-configs.ts` 给 DeepSeek
   网页补了静态表单（令牌 + 可选 Cookie）；
3. **兜底**：没有静态配置的插件家仍会退到「手动凭证 JSON」，不会再死路。

## 3. 添加账号（以通义千问网页为例）

1. 浏览器打开 <https://chat.qwen.ai> 并登录；
2. F12 → Application → Local Storage → `chat.qwen.ai` → 复制 `token`
   （`eyJ` 开头的 JWT，**不是** API Key）；
3. 面板 → 账号 → 添加账号 → 「网页」→「通义千问 网页」；
4. 粘贴令牌（Cookie 可留空），保存；
5. 网关会用 `Cookie: token=<JWT>` 调 `GET /api/v1/auths/` 校验，通过后
   账号出现在列表里。

> 令牌等同于账号密码：数据库里加密存放（`enc1:` 前缀），不要贴到聊天/截图里。

## 4. 扩展包 `agent2api-web-extra` 怎么来的

官方网页插件包只带 chatglm + deepseek-web。新家（千问）按插件协议单独做了
一个包，源码在 `contrib/web-extra`（fork 自 `ProvidersPlugin/Agent2WebProviders`，
只保留 qwen 模块）：

```bash
# 编译（宿主机没有 Rust 时用 Docker；依赖走 GitHub，必须 --network host）
docker run --rm --network host \
  -v "$PWD/contrib/web-extra:/src" -v a2a-cargo-cache:/cargo -w /src \
  -e CARGO_HOME=/cargo -e CARGO_PROFILE_RELEASE_LTO=false \
  rust:1.88-bookworm cargo build --release -p agent2api-web-providers \
  --no-default-features --features qwen

# 打包
mkdir -p /tmp/web-extra/bin
cp contrib/web-extra/target/release/agent2api-web-providers /tmp/web-extra/bin/
cp contrib/web-extra/pack.json /tmp/web-extra/
(cd /tmp/web-extra && zip -qr ../agent2api-web-extra-1.0.0-x86_64-unknown-linux-gnu.zip pack.json bin)

# 安装（面板登录后）
curl -b panel.cookie -X POST --data-binary @agent2api-web-extra-1.0.0-x86_64-unknown-linux-gnu.zip \
  -H 'content-type: application/octet-stream' http://127.0.0.1:3066/api/plugins/install
```

包没有官方签名，安装时界面会标黄「未签名」——这是允许的（本地自建包），
装完在「插件」页确认 `agent2api-web-extra` 已启用即可。

## 5. 再想加一家（Kimi / 豆包 / 文心…）要做什么

1. 在 `contrib/web-extra/crates/providers/src/providers/` 下新建模块，
   实现 `Provider`（`meta` / `conversation` / `normalize_account` / `models` /
   `classify`）；
2. `mod.rs` 注册、`Cargo.toml` 加 feature、`pack.json` 的 `providers` 加一项；
3. 按上面的命令编译、打包、安装；
4. 界面表单：没有静态配置时会退到「手动凭证 JSON」；要好看的表单就在
   `add-account-configs.ts` 里加一张 `ProviderConfig`（参考 `QWEN_WEB`）。

> 各家网页协议随时会变；实现前先找一份**近期还在维护**的开源实现核对
> 端点、请求体与 SSE 形态，别照抄过期的博客。
