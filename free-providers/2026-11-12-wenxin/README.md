# free-wenxin · 文心（百度千帆）反代插件

给 Agent2API 用的文心反代：一条 provider 承载百度千帆的**两代鉴权面**。

| | API Key 模式（主推） | AK/SK 模式（旧版兼容） |
|---|---|---|
| 端点 | `POST https://qianfan.baidubce.com/v2/chat/completions` | `POST https://aip.baidubce.com/rpc/2.0/ai_custom/v1/wenxinworkshop/chat` |
| 鉴权 | `Authorization: Bearer <bce-v3/ALTAK-…>`（永久有效） | AK/SK 换 `access_token`（30 天，可程序化重取） |
| 协议 | OpenAI 兼容，SSE 以 `data: [DONE]` 结束 | 自有信封（`result`/`is_end`/`error_code`，HTTP 200 也可能是失败） |
| 目录 | `GET /v2/models`（动态拉取，按 `type`/`endpoints`/`status` 过滤） | 无目录接口，静态接入点名表 |

能力位：`chat` ✓ · `stateful` ✓ · `refresh` ✓（仅 AK/SK） · `model_refresh` ✓（仅 API Key） ·
`usage` ✗ · `checkin` ✗ · `web_login` ✗ —— 每个 ✗ 都是「上游没有」，
排除过程写在 `crates/providers/src/wenxin/mod.rs` 的模块头与 `SELFTEST.md` §3。

## 跑起来

```bash
cargo build --release --offline     # 产出 target/release/free-wenxin
python3 selftest/selftest.py        # 帧级自测（本机 mock 上游，52 项）
python3 selftest/e2e_gateway.py     # 端到端自测（真网关加载插件包，21 项）

# 打包 + 安装到生产网关（需重启网关生效）
./install.sh --home <数据目录>       # 如本仓容器网关：--home ./data
docker restart <网关容器名>
```

配置示例（占位符 + 环境变量，**不含真实密钥**）、能力位依据、错误映射表、
已知限制、自测原始输出：见 **[`SELFTEST.md`](SELFTEST.md)**。
接口事实来源：仓库 `docs/research/qianfan-ernie-api-surface.md`。

## 真实验证状态（2026-11-12）

| # | 验收项 | 结果 |
|---|---|---|
| 1 | 构建可复现（`cargo build --release --offline`） | ✅ 通过 |
| 2 | 帧级自测 52 项（mock 上游，不碰真网） | ✅ 52/52 通过 |
| 3 | 端到端自测 21 项（隔离网关实例 + 真 HTTP 面） | ✅ 21/21 通过 |
| 4 | 生产容器网关加载（`agent2api-fork` 重启后日志确认） | ✅ `插件包「free-wenxin」v2026.11.12 已加载，1 家提供商可用` |
| 5 | 真上游可达 + 错误信封与判型一致 | ✅ 无 Key 打 `/v2/models` → `AccessDenied`；无效 Key 对话 → `invalid_iam_token`（插件判 `TokenExpired`，与模块头一致） |
| 6 | 真 Key 对话（`/v1/chat/completions` 走网关） | ⏳ **待一把真 Key**：千帆 API Key（`bce-v3/ALTAK-…`）或 AK/SK。给了之后跑 `scripts/probe-provider.mjs --provider wenxin --model <模型名>` 全用例 |

第 6 项是唯一缺口：插件 `account.normalize` 会拿 Key 打真上游校验，
假 Key 拒绝落库是**设计行为**（不是 bug），所以生产网关现在 `/v1/models`
里还没有文心条目 —— 加一把真账号即出现。
