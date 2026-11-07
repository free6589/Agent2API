# agent2api-web-extra · 通义千问网页扩展包

`Agent2API` 的**第三方网页提供商插件包**：把通义千问网页版（`chat.qwen.ai`）
的本人登录态接进网关，作为 `qwen-web` 提供商参与转发。官方网页包
（`agent2api-web-providers`，含智谱清言 / DeepSeek / Tabbit）不包含千问网页，
本包补上这一家。

> 仓库主线（`beta` / `main`）不构建本包；它按「插件包 → 从文件安装」的方式
> 交付。许可证与第三方来源见包内 `LICENSE` 与 `NOTICE.md`。

## 提供的能力（按申报为准）

| 能力 | 申报 | 说明 |
| --- | --- | --- |
| 对话（含流式） | ✓ | 建会话 + SSE，`delta.phase` 区分思考与正文；锁第一个候选 index |
| 工具调用 | 模拟 | 无原生协议；目录申报 `toolCallMode: "emulated"`，`<tool_call>` 约定 |
| 模型列表 | ✓ | 静态兜底 + 登录态拉 `/api/models` |
| Token 自动续期 | ✗ | 网页 token 是 JWT，无公开续期接口；过期后回官网重登再更新 |
| 余额 / 签到 | ✗ | 上游无对应接口，不申报 |

## 构建

依赖：Rust 1.88+（版本钉在 SDK tag `v3.4.0`，与网关 `server/Cargo.toml` 一致）。

```bash
cd contrib/web-extra
cargo build --release --target x86_64-unknown-linux-gnu
```

五平台 target 与 `pack.json` 的 `entry` 一一对应：`x86_64-pc-windows-msvc`、
`x86_64-unknown-linux-gnu`、`aarch64-unknown-linux-gnu`、`x86_64-apple-darwin`、
`aarch64-apple-darwin`（Windows 产物多 `.exe` 后缀）。

## 打包与安装

zip 根目录放 `pack.json`，二进制放 `bin/<crate 名>`（与 `entry` 一致）：

```bash
cd contrib/web-extra
T=x86_64-unknown-linux-gnu
cargo build --release --target $T
mkdir -p bin && cp target/$T/release/agent2api-web-extra bin/
zip -r ../../agent2api-web-extra-1.1.0-$T.zip pack.json LICENSE NOTICE.md bin
```

面板 →「插件」→「从文件安装」选择该 zip。本包是第三方包（不在
`OFFICIAL_PACKS` 名单内），没有自动更新：升级时重新打包并在插件页替换安装。

## 添加账号

装好插件后，「添加账号 → 通义千问 网页」会读到插件申报的动态登录表单：

1. 在 `chat.qwen.ai` 登录本人账号；
2. 浏览器开发者工具 → Application → Local Storage，复制 `token` 的 value
   （`eyJ` 开头的 JWT；**不是**收费 API Key）；
3. 粘贴到「网页访问令牌」；可选粘贴整段 Cookie（风控期更稳）；
4. 提交后插件会调 `/api/v1/auths/` 校验登录态，无效令牌会明确报错。

登录态失效或上游要求人机验证时，账号会报「请回官方网页完成验证后更新
token」——按设计如实透传，不做绕过。

## 排错

- 「千问建会话失败」→ token 过期，回官网重登后更新凭证；
- 「千问网页要求人机验证」→ 在官方网页完成验证后更新 token；
- 「千问网页版今日配额已耗尽」→ 上游限额，次日恢复；
- 工具调用失败重试仍失败 → 该模型对 `<tool_call>` 约定的遵从度问题，换
  `qwen3-max` 或 `qwen3-coder-plus` 再试。
