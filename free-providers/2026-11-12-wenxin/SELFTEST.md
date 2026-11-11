# free-wenxin 自测与配置说明

> 百度文心（千帆）反代插件。两条鉴权面：**API Key 模式**（`qianfan.baidubce.com/v2`，
> OpenAI 兼容，主推）与 **AK/SK 模式**（`aip.baidubce.com` 的 wenxinworkshop，
> 用 `access_token`，可程序化续期）。接口依据见仓库
> `docs/research/qianfan-ernie-api-surface.md`（2026-11-12 实抓官方文档）。

## 1. 自测（三条，都可复现）

```bash
cd free-providers/2026-11-12-wenxin
cargo build --release --offline          # 依赖已缓存在 ~/.cargo/git，可离线构建

python3 selftest/selftest.py             # 帧级：直接对插件二进制讲网关协议
python3 selftest/e2e_gateway.py          # 端到端：真网关加载插件包 → 加账号 → /v1 调用
./install.sh --home <数据目录>           # 安装到生产网关（幂等、可回滚）
```

`e2e_gateway.py` 需要先起一个**隔离的网关实例**（不碰 `~/.agent2api`，也不碰
3065 上的生产实例）：

```bash
export AGENT2API_PROXY_HOME="$PWD/.scratch-tmp/wx-gw"
export AGENT2API_PROXY_PORT=3066 AGENT2API_HOST=127.0.0.1
export AGENT2API_ALLOW_NO_KEY=1 AGENT2API_CAPTCHA_ENABLED=0
# 插件包按官方布局落盘：$AGENT2API_PROXY_HOME/plugins/free-wenxin/2026.11.12/
#   pack.json / bin/free-wenxin / assets/
# 并在 $AGENT2API_PROXY_HOME/plugins/plugins.json 里登记该包（见 install.rs 的 InstalledPack）
./agent2api-server &
```

### 1.1 原始输出（跑出来的，不是手写）

| 文件 | 内容 | 结果 |
|---|---|---|
| `selftest/out/selftest.log` | 帧级自测全量输出 | **52 项通过，0 项失败** |
| `selftest/out/e2e-gateway.log` | 真网关端到端输出 + 网关侧日志原文 | **21 项通过，0 项失败** |

端到端日志里网关自己打的这三行是「被应用发现并加载」的直接证据：

```
Plugin 插件包「free-wenxin」v2026.11.12 已加载，1 家提供商可用
[Upstream] 会话式转发 model=ernie-5.1 stream=false … provider=wenxin
[Upstream] 会话式转发 model=ernie-5.1 stream=true  … provider=wenxin
```

### 1.2 两套自测各证明什么

**帧级（`selftest.py`，自带 mock 上游）**

- `init` 握手返回 `wenxin` 的 ProviderInfo，能力位与申报一致；
- `models.list` 静态兜底 → `models.refresh` 真打 `/v2/models` 并**按
  `type=="chat"` / `endpoints` / `status` 过滤**（embeddings、下线、不挂 chat
  端点的条目必须被丢掉）；`features` 含 `function_call` 才给 `supportsToolCall`；
- `conversation` 流式：head 200 → 多个 chunk → `result: null` 收尾，帧是
  OpenAI `chat.completion.chunk`，末帧带非零 usage，以 `data: [DONE]` 结束；
- 上游**忽略 stream 参数整包回 JSON** 时，插件翻成「正文 + 收尾 + usage」帧流
  （网关给非流式客户端聚合时消费的就是这个形状）；
- 上游 401 时 head 带状态码与原始错误体（网关据此调 `classify`）；
- `classify` 对官方错误码表逐族给出正确四档（11 个用例，见日志）；
- AK/SK 模式：`token.ensure` 打 oauth 换 token 并**比较-再写回**；
  `token.expiring` 临期判据（30 天有效期 / 3 天窗口）；`token.refresh` 强制重取；
  V1 的 `result`/`is_end` 帧被翻成 OpenAI 帧并补 `[DONE]`；
- 账号规范化：API Key 被上游 401 时**拒绝落库**；公开形态不含凭证本体。

**端到端（`e2e_gateway.py`，真网关 + 真 HTTP 面）**

- `/api/session` 的家清单里出现 `wenxin`，标签 `文心（千帆）`，`source: "plugin"`；
- 添加账号表单由插件申报驱动（`loginMethods` 里是 `token` 表单 + `secretKey` 附加字段），
  账号行渲染参数也来自插件（`identifier: ["authMode"]`、`usage: false`）——
  **前端不需要为这家加任何代码**；
- 网关**真的 spawn 了插件入口二进制**（`/proc/*/exe` 命中 `free-wenxin`）；
- `POST /api/accounts` 走插件 `account.normalize` 落库，账号行由插件
  `account_view` 产出（`authMode: "API Key"`），列表里**没有凭证本体**；
- `/v1/models` 出现 `owned_by: wenxin` 的 15 条模型，含 `ernie-5.1`；
- `POST /v1/chat/completions`（非流式）→ 正文「你好」+ `usage.total_tokens=11`；
- 同端点 `stream: true` → 逐块下发、拼出「你好」、以 `[DONE]` 结束；
- 上游会拒的 Key 加账号 → 被插件拦下，不落库。

## 2. 配置示例

凭证**只**进账号记录（`accounts.data.secret`），插件定义 / `pack.json` / 日志
里永不出现；公开形态只有 `tokenTail`（尾 4 字符）与 `authMode`。

| 文件 | 模式 |
|---|---|
| `examples/account.api-key.example.json` | API Key（主推） |
| `examples/account.ak-sk.example.json` | 旧版 AK/SK（`accessToken` 由插件回写） |

占位符与环境变量（**任何提交文件里只允许出现变量名**）：

| 占位符 | 环境变量 | 从哪拿 |
|---|---|---|
| `${QIANFAN_API_KEY}` | `QIANFAN_API_KEY` | 百度智能云控制台 → 安全认证 API Key（`bce-v3/ALTAK-…`） |
| `${QIANFAN_APP_API_KEY}` | `QIANFAN_APP_API_KEY` | 千帆应用详情里的 API Key（旧版 AK/SK 模式） |
| `${QIANFAN_APP_SECRET_KEY}` | `QIANFAN_APP_SECRET_KEY` | 同上，Secret Key |

面板粘贴是主路径；命令行添加时从环境变量读，**不要把 key 写进命令历史**：

```bash
read -rs QIANFAN_API_KEY && export QIANFAN_API_KEY   # 交互输入，不回显
curl -sS -X POST http://127.0.0.1:3065/api/accounts \
  -H "Content-Type: application/json" \
  -d "$(python3 -c 'import json,os;print(json.dumps({"provider":"wenxin","name":"文心","apiKey":os.environ["QIANFAN_API_KEY"]}))')"
unset QIANFAN_API_KEY
```

`baseUrl` 可选（默认 `https://qianfan.baidubce.com/v2`；AK/SK 模式默认
`https://aip.baidubce.com`），自测时用它指到本机 mock 上游。

## 3. 能力位与「没有某接口」的排除过程

| 能力位 | 值 | 依据 |
|---|---|---|
| `chat` | ✓ | `POST /v2/chat/completions` |
| `stateful` | ✓ | 走 `conversation`：V1 非 OpenAI 协议必须插件翻译，两条模式共用一条出口 |
| `refresh` | ✓（仅 AK/SK） | oauth `client_credentials` 换 `access_token`，30 天、可随时重取 |
| `model_refresh` | ✓（仅 API Key） | `GET /v2/models`；V1 的模型编码在 URL 路径里，**官方 v1 文档全文无清单端点** |
| `usage` | ✗ | 千帆 v2 推理面**没有**余额/用量端点；计费类接口要百度云 AK/SK 的 `bce-auth-v1` 签名（另一套凭证族），用 API Key 打必 401 → 不给入口 |
| `checkin` | ✗ | 千帆没有签到 / 每日领取 / 新手礼这类可程序化动作（控制台「量包/代金券」是人工购买与财务流程） |
| `web_login` | ✗ | 没有网页授权换凭证链路，凭证是控制台复制的静态 Key / AK+SK |

## 4. 错误映射（判据 = 官方错误码表，2026-11-11 快照）

V2 信封 `{"error":{"code","message","type"},"id":"as-…"}`：

| 上游 | 归类 | 处置 |
|---|---|---|
| `type=unsafe_request` / `system_unsafe` / `user_setting_unsafe` / `functions_unsafe` / `image_url_unsafe` | `ContentBlocked` | **不罚账号**，换中性提示词重试 |
| `account_overdue`（欠费） | `QuotaLimited`（短冷却） | 充值即恢复，所以不给「次日零点」冷却 |
| `coding_plan_{hour,week,month}_quota_exceeded` | `QuotaLimited` | 滚动窗口，上游不给恢复时刻 |
| 429 / `type=rate_limit_exceeded` | `QuotaLimited` | 分钟级窗口，换账号可继续 |
| `invalid_iam_token` | `TokenExpired` | 刷新后同账号重试一次 |
| 其余 401/403（`invalid_model` 无模型权限 / `invalid_appid` / `no_parameter_permission` / `model_offline`） | `Fatal` | 重刷无用，如实透传（判成过期会误导用户重贴一把好 Key） |

V1（HTTP 200 + `error_code`）：`110`/`111` → `TokenExpired`（110 有官方原文）；
`4/17/18/19` → `QuotaLimited`；`13/14` → `TokenExpired`；其余 → `Fatal`。
`4/17/18/19`、`13/14` 来自百度开放平台错误码表的长期口径，本轮未抓到该页原文
（页面 JS 渲染）—— 取舍方向是「宁可多换号，不要卡死」：限额误判成 `Fatal`
会让账号永不轮换，反过来只会冷却 10 分钟。

## 5. 已知限制

1. **V1 已公告下线**：官方说 V1 推理接口即将下线，AK/SK 模式是兼容面而非长期面；
   新接入请用 API Key 模式。V1 也没有 function calling / 多模态 / `stream_options`，
   插件对这些请求**明确报错**而不是静默丢字段。
2. **V1 的模型清单是静态表**（接入点后缀），没有目录接口可拉；新增旧版模型要改
   `models::static_v1()`。
3. **没有余额 / 签到 / 领取**：见第 3 节的排除过程，不是待办。
4. **`supportsImages` 静态为 false**：v2 的多模态是按模型的能力
   （`architecture.modality`），静态表不按 id 猜；远程目录拉到 `input_modalities`
   时才置真。
5. **思考开关只在别名后缀可解析时注入**：请求名带 `-thinking` / `-nothinking`
   且剥后缀后的 id 在目录里认得出，才注入 `thinking.type`；客户端自己带了
   `thinking` / `enable_thinking` / `reasoning_effort` 时**一字不动**。
6. **`models.refresh` 走网关默认出口**（该 RPC 没有账号代理入参），账号级代理只
   作用于对话。
7. **未签名包**：`pack.json` 不带 `signature`，网关会按「第三方未签名」标黄
   （允许安装，这是自建包的正常形态）。
8. **`自测不碰真上游**：前两条自测都用本机 mock + 假 Key；真实可用性见 README
   「真实验证状态」表 —— 生产网关加载与真上游连通性已验证，
   真 Key 对话待补（VERIFY 三步，见仓库 `free-providers/VERIFY.md`）。

## 7. 生产网关安装验证（2026-11-12，`agent2api-fork` 容器）

- 数据目录即本仓 `data/`（容器挂载 `/data ← 本仓 data/`）；
- `install.sh` 逻辑已手工执行：`data/plugins/free-wenxin/2026.11.12/`
 （`pack.json` + `bin/free-wenxin`）+ `data/plugins/plugins.json` 登记；
- `docker restart agent2api-fork` 后日志原文：
  `Plugin 插件包「free-wenxin」v2026.11.12 已加载，1 家提供商可用`；
- 无账号时 `/v1/models` 无文心条目是**预期行为**（fail-closed，
  `probe-provider.mjs` 前置校验直接退出码 2，不误报）；
- 真 Key 到位后的收尾动作（一条命令，加号 + 全用例探测）：

```bash
curl -sS -X POST http://127.0.0.1:3065/api/accounts \
  -H "Content-Type: application/json" \
  -d '{"provider":"wenxin","name":"文心","apiKey":"'"$QIANFAN_API_KEY"'"}'
node scripts/probe-provider.mjs --provider wenxin --model ernie-5.1 \
  --base http://127.0.0.1:3065 --key <只允许wenxin的网关Key>
```

## 6. 红线

- 本目录所有文件**不含任何真实密钥**；示例只用 `${VAR}` 占位符。
- 自测的 mock 上游只监听 `127.0.0.1`，不出网。
- 端到端自测用的是**隔离数据目录**的网关实例（`AGENT2API_PROXY_HOME`），
  管理员口令是运行时随机生成的一次性口令，不打印、不落盘。
