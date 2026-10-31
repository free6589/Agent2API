# 上游 Issue / PR 审计记录（beta.6 基线）

- 审计对象：`aimod-cc/Agent2API`
- 基线：`upstream/beta` = `bd36938d7cc32f2a372a63e57f3aec873cf53f11`（v3.3.0-beta.6）
- 审计日期：2026-10-31（UTC）
- 审计方式：GitHub REST/GraphQL 分页读取全部 49 个 open issue、23 个 open PR（搜索 API 只返回 17 个 PR，REST `/pulls?state=open` 才是准确的）；所有 PR 都对 beta 执行 `git apply --check`；另核查与部署/API 相关的已关闭 issue/PR。
- 限制：审计本身没有运行 Docker、cargo test 或登录真实账号；本仓库的实际构建、测试、部署与真实模型调用结果记录在 [`OMARCHY_LINUX.md`](./OMARCHY_LINUX.md)。因此下表的“仍存在/未复现”是代码或报告证据判断，不等于本机运行时复现。

## 结论摘要

1. **beta 是正确代码线**：`main` 为旧稳定线 v3.0.8，beta 比 main 多 30 个提交且没有反向缺失；本地部署必须基于 beta。
2. **49 个 open issue、23 个 open PR**；绝大多数 PR 是 provider 功能/旧架构分支，不能直接合并。
3. **可安全整合的 PR 只有 #305（SenseNova 预置 URL）和 #311（后端 CI）**。本次已实际应用并验证（见文末）。
4. **beta.6 仍存在的 P0/P1 核心缺陷**（不是“缺 PR”，而是代码本身）：
   - **#407**：凭证刷新只判断 `Ok`，不校验 `updatedAt`/`expiresAt` 是否推进。本分支已修复并加回归测试。
   - **#404**：插件自动更新期间关闭 latch 会让在途请求被误判 Fatal，建议后续单独修复。
   - **#363**：插件所有权判断忽略“逐家停用”开关，建议后续单独修。
   - **#408**：候选路由没有按 `supports_images`/`input_modalities` 过滤带图请求，建议后续单独修。
   - **#401**：旧 v20 SQLite 在 Linux 上启动崩溃被关闭但无代码证据；本次未用 v20 数据库复现，不能宣称已修。
   - **#403/#375**：cookie-probe 契约与桌面 `tauri://` 相对 `/api` 请求仍有缺口；本次部署走的是浏览器面板，不依赖桌面形态。
5. **已关闭但相关的记录**：#391（Docker beta.5 更新 panic）已在 beta.5.1 的 `83c4a37` 修复；#403 的登录循环被 `?from=login` 缓解但 probe 契约未修；#375 桌面登录页仍在代码中保留相对 fetch；#359 的 UI 部分已修、容器 `/tmp` 持久化需插件侧验证。

## 重点缺陷逐条核查（部署 / API 相关）

| 编号 | 问题 | 复现或代码证据 | beta.6 当前状态 | 对 Omarchy/API 的影响 | 本分支处置 |
| --- | --- | --- | --- | --- | --- |
| [#407](https://github.com/aimod-cc/Agent2API/issues/407) | 凭证刷新不校验结果，`Ok` 即记成功 | `core/credential_maintenance.rs` 与 `api/accounts.rs` 的 `Ok(_)` 分支都不读刷新后的账号；issue 给出 7 个 AutoClaw 账号 28 小时空刷、`updatedAt` 冻结的实测 | 仍存在（本次代码检查）；本机手动刷新一个 AutoClaw 账号时 `updatedAt` 有推进，说明该账号本次不是空刷，但通用缺口仍在 | 账号过期后日志全绿、实际不可用；自动维护与手动刷新都会误报 | **已修复**：新增 `credential_stamp`/`credentials_advanced`，自动维护将空刷记为 `failed`，手动接口返回 409；新增 4 个单元测试 |
| [#404](https://github.com/aimod-cc/Agent2API/issues/404) | 插件自动更新期间在途请求被误判 Fatal | `plugin_host/lifecycle.rs` 在 `closing` 后返回“插件宿主已关闭…”；`runtime.rs` 先 await shutdown 再 detach，窗口期表仍指向 closing host | 仍存在（代码检查） | 自动更新时正在转发的请求会收到不可重试错误，可能中断用户对话 | 记录为 P0，建议后续把 detach 提前到 await 之前并把该错误映射为可重试；本次未改，避免无并发测试下扩大改动 |
| [#363](https://github.com/aimod-cc/Agent2API/issues/363) | 所有权判断忽略逐家停用开关 | `plugin_host/ownership.rs` 只过滤 `pack.enabled`，未读 `pack.providers`；`bootstrap.rs` 在选 owner 之后才检查开关 | 仍存在（代码检查） | 两个插件包声明同一提供商时，已停用的包仍占 owner，另一个包无法加载 | 记录为 P0，建议在 `owners()` 过滤 `pack.providers` 并加 tie-break 测试；本次未改 |
| [#408](https://github.com/aimod-cc/Agent2API/issues/408) | `supports_images` 未参与候选过滤 | `providers/catalog/routing.rs` 无 `supports_images`/`input_modalities`；`provider_loop.rs` 只过滤图片生成模型 | 仍存在（代码检查） | 带图请求会投给纯文本模型，白重试最多 9 跳后才失败；API 客户端看到 502 而非明确 400 | 记录为 P1，建议按 manifest 能力过滤；本次未改，避免影响路由主路径 |
| [#401](https://github.com/aimod-cc/Agent2API/issues/401) | Linux 旧 v20 SQLite 启动 exit 134 | issue 有两台机器复现；维护者仅回复“beta.6 已发布”，没有对应提交；beta.6 仍有 20→21→22 迁移 | **未证实已修** | 从旧版本升级的 Linux 用户可能在启动时崩溃；本次没有 v20 数据库可复现 | 明确标为未验证风险；升级前备份，必要时用 beta.6 + `RUST_BACKTRACE=1` 复现 |
| [#403](https://github.com/aimod-cc/Agent2API/issues/403) | cookie-probe 契约不一致导致登录循环 | `ui/login.html` 要求 `token` 和 `tokenB`；`api/panel.rs::cookie_probe` 只返回 `token`，`expect-b` 会序列化 `undefined` | 仍存在；`?from=login` + token-in-body 兜底让常见网页登录可用 | 浏览器登录可用，但 cookie 探测永远走 fallback；不是启动阻断 | 本次浏览器面板登录验证通过；不在本次改认证契约，记录为后续 |
| [#375](https://github.com/aimod-cc/Agent2API/issues/375) | 桌面登录页相对 `/api` 请求打不到网关 | `login.html:240` 仍 `fetch('/api/panel/status')`；Tauri 页面源 `tauri://localhost`，无 `/api` asset proxy | 仍存在（代码检查） | 只影响原生 Tauri 桌面；容器/浏览器面板不受影响 | 本次未构建原生桌面，明确不声称已修 |
| [#360](https://github.com/aimod-cc/Agent2API/issues/360) | refresh token 已吊销仍无限重试、不落 `needsRelogin` | `credential_maintenance.rs` 只在 `needs_relogin` 为真时跳过；`provider_loop` 仅对不支持刷新者设置该标记 | 仍存在（代码检查） | 凭证已吊销的账号每轮维护都打上游、日志噪声，用户看不到“请重新登录” | 记录为 P1；本次未改，避免误判各家 refresh 错误语义 |
| [#301](https://github.com/aimod-cc/Agent2API/issues/301) | 账号级 `baseUrl` 覆盖无 UI/API | `custom/forward.rs` 优先 `base_url_override`，但 `PATCH /api/accounts/{id}` 白名单不含 `baseUrl` | 仍存在（代码检查） | 用户改提供商 baseUrl 后，旧账号覆盖仍打旧地址；自定义提供商场景 | 记录为 P1；本次未改，需同时补 UI 与 API 契约 |
| [#260](https://github.com/aimod-cc/Agent2API/pull/260) | 折过的 413 与 Loomy 中文长度墙漏网 | beta.6 `loomy::classify_error` 无 `内容太长` 分支；`PAYLOAD_SIZE_REJECTION_STATUSES` 只有 413 | 逻辑仍缺；PR 与 beta 冲突 | 大请求会重复发送/错误分类，影响任意 Linux 部署的 API 稳定性 | 记录为可移植补丁；本次未合并，保留 PR 证据 |
| [#305](https://github.com/aimod-cc/Agent2API/pull/305) | SenseNova 预置 base URL 错误 | beta.6 仍为 `https://api.sensenova.cn/compatible-mode/v1`，PR 改为 `https://token.sensenova.cn/v1` | PR 干净可应用 | 仅影响选择 SenseNova 预置的用户 | **已应用**（1 行） |
| [#311](https://github.com/aimod-cc/Agent2API/pull/311) | 后端 594 个测试没有 CI 执行 | `.github/workflows/` 无 server-tests；本地测试目标还因两处陈旧初始化无法编译 | PR 干净可应用；本分支先修复测试编译 | 防止未来的选路/凭据/错误码回归 | **已应用**；另修 `RequestQuery` 与 `send_with_retry` 的测试调用 |

## 全部 49 个 open issue

| # | Title (short) | Category | Deploy/API? | Verdict |
|---:|---|---|---|---|
| [122](https://github.com/aimod-cc/Agent2API/issues/122) | “No ads in issues” admin notice | Meta | No | Ignore; not a product issue. |
| [153](https://github.com/aimod-cc/Agent2API/issues/153) | WPS Lingxi provider request | Provider request | No | Feature request; external provider work, not deployment. |
| [164](https://github.com/aimod-cc/Agent2API/issues/164) | TRAE international provider request | Provider request | No | Request; Trae is now plugin-delegated (`missing_plugin.rs`). |
| [171](https://github.com/aimod-cc/Agent2API/issues/171) | MiniMax Code CN / LobsterAI request | Provider request | No | Feature request; LobsterAI has account-store code but provider is plugin. |
| [173](https://github.com/aimod-cc/Agent2API/issues/173) | Model scheduling is a black box | UX/routing | Indirect | Valid observability feature; not a blocker. |
| [184](https://github.com/aimod-cc/Agent2API/issues/184) | 3.0.1 account warning | Support/stale | No | Needs version/logs; stale. |
| [196](https://github.com/aimod-cc/Agent2API/issues/196) | Account groups / routing groups | Feature | No | Large feature; overlaps #321; not deployment. |
| [200](https://github.com/aimod-cc/Agent2API/issues/200) | Empty bug report (3.0.1) | Stale | No | Insufficient information. |
| [208](https://github.com/aimod-cc/Agent2API/issues/208) | Model mapping case-sensitivity | Config bug | Indirect | Minor mapping/UI bug; low priority. |
| [209](https://github.com/aimod-cc/Agent2API/issues/209) | MiniMax Code provider | Provider request | No | Feature request. |
| [216](https://github.com/aimod-cc/Agent2API/issues/216) | API-key scope / account-model isolation | Feature | No | Product feature; not required for local deployment. |
| [244](https://github.com/aimod-cc/Agent2API/issues/244) | Tencent ima provider | Provider request | No | Feature request. |
| [245](https://github.com/aimod-cc/Agent2API/issues/245) | iFlytek Astudio provider | Provider request | No | Feature request. |
| [248](https://github.com/aimod-cc/Agent2API/issues/248) | Adaptive daily quota limiter | Feature | No | Product feature; large design. |
| [272](https://github.com/aimod-cc/Agent2API/issues/272) | Notifications | Feature | No | Product feature. |
| [275](https://github.com/aimod-cc/Agent2API/issues/275) | Per-account API key limits | Feature | No | Product feature. |
| [276](https://github.com/aimod-cc/Agent2API/issues/276) | Selective account export / file import | Feature | No | Product feature; account export exists partially. |
| [293](https://github.com/aimod-cc/Agent2API/issues/293) | Route runtime-state clear endpoint | Ops feature | Indirect | Useful after long-running gateway; not startup. |
| [295](https://github.com/aimod-cc/Agent2API/issues/295) | Transparent stream resume / failover | Feature | Indirect | Large streaming feature; not deployment. |
| [301](https://github.com/aimod-cc/Agent2API/issues/301) | Custom provider account `baseUrl` override has no UI | Config/forwarding bug | Yes | **Real bug; high-value fix.** See P1. |
| [303](https://github.com/aimod-cc/Agent2API/issues/303) | Model mapping primary/backup | Routing feature | Indirect | Valid enhancement; not a blocker. |
| [308](https://github.com/aimod-cc/Agent2API/issues/308) | Assistant `list_models` misses custom providers | API/UI bug | Indirect | Real but assistant-only; low priority vs #407/#404. |
| [318](https://github.com/aimod-cc/Agent2API/issues/318) | Unsupported sampling params silently dropped | API compatibility | Indirect | Valid observability/doc gap. |
| [319](https://github.com/aimod-cc/Agent2API/issues/319) | CodeArts invalid channel / group not found | Provider external | No | Provider/plugin account issue; need plugin logs. |
| [323](https://github.com/aimod-cc/Agent2API/issues/323) | OrcaTerm tool calls broken | Provider external | No | OrcaTerm plugin; reproduce with plugin, not core. |
| [324](https://github.com/aimod-cc/Agent2API/issues/324) | Qoder CN 183 s EOF, status 200/tokens 0 | Provider + stats | Indirect | Provider plugin issue; generic incomplete-stream accounting bug is real. |
| [325](https://github.com/aimod-cc/Agent2API/issues/325) | Claude Desktop / AutoClaw 406 support question | Support | No | Not a bug report with reproducible gateway failure. |
| [327](https://github.com/aimod-cc/Agent2API/issues/327) | Raccoon 1-token truncation; save finish_reason | Observability | Indirect | Valid low-risk improvement; root cause was client `max_tokens=1`. |
| [329](https://github.com/aimod-cc/Agent2API/issues/329) | Report “today” uses wrong boundary | Stats bug | Indirect | Real but low severity; add midnight test. |
| [342](https://github.com/aimod-cc/Agent2API/issues/342) | ZCode captcha token throughput | Performance feature | No | Product/ops feature; not deployment. |
| [356](https://github.com/aimod-cc/Agent2API/issues/356) | Free-tier accounts see paid models | API/catalog bug | Indirect | Real API UX issue; needs tier capability or switch. |
| [360](https://github.com/aimod-cc/Agent2API/issues/360) | Revoked refresh token retries forever | Auth/credential bug | Yes | **Real bug; fix `needsRelogin`.** See P0. |
| [363](https://github.com/aimod-cc/Agent2API/issues/363) | Ownership ignores per-provider disable | Plugin startup bug | Yes | **Real bug; fix `owners()`.** See P0. |
| [364](https://github.com/aimod-cc/Agent2API/issues/364) | Trae referral points request | Feature | No | Product feature. |
| [367](https://github.com/aimod-cc/Agent2API/issues/367) | orcaRouter provider request | Provider request | No | Feature request. |
| [371](https://github.com/aimod-cc/Agent2API/issues/371) | Super Maggie provider request | Provider request | No | Feature request. |
| [373](https://github.com/aimod-cc/Agent2API/issues/373) | Stream gate: “switch provider without cooldown” | Routing feature | Indirect | Valid design request; not a blocker. |
| [378](https://github.com/aimod-cc/Agent2API/issues/378) | Batch model validation button | UI feature | No | Product feature. |
| [390](https://github.com/aimod-cc/Agent2API/issues/390) | minimaxcode provider request | Provider request | No | Incomplete request. |
| [400](https://github.com/aimod-cc/Agent2API/issues/400) | Omniroute-style custom combinations | Feature | No | Product feature. |
| [402](https://github.com/aimod-cc/Agent2API/issues/402) | Gemini provider request | Provider request | No | Feature request. |
| [404](https://github.com/aimod-cc/Agent2API/issues/404) | Plugin auto-update makes in-flight requests Fatal | Plugin lifecycle bug | Yes | **Real bug; P0 fix.** |
| [405](https://github.com/aimod-cc/Agent2API/issues/405) | Accio fails on beta.6 | Provider external | No | Provider/plugin; need request/log. |
| [406](https://github.com/aimod-cc/Agent2API/issues/406) | Yunzhisheng MaaS provider request | Provider request | No | Feature request. |
| [407](https://github.com/aimod-cc/Agent2API/issues/407) | Credential refresh does not verify result | Auth/credential bug | Yes | **Real bug; P0 fix; local candidate patch exists.** |
| [408](https://github.com/aimod-cc/Agent2API/issues/408) | `supports_images` not used in candidate filtering | Routing/API bug | Yes | **Real bug; P0 fix.** |
| [410](https://github.com/aimod-cc/Agent2API/issues/410) | Marvis / QClaw / MiniMax Code check-in request | Provider request | No | Feature request. |
| [413](https://github.com/aimod-cc/Agent2API/issues/413) | MonkeyCode account can be added but not used | Provider external | No | Beta.6 report; need plugin logs/account. |
| [416](https://github.com/aimod-cc/Agent2API/issues/416) | CatPaw tool schema depth>12 rejects MCP tools | Provider external | No | Real plugin bug; external/manual validation. |

**统计：** 49 个 open issue；其中 **6 个直接影响部署/API**（#301、#360、#363、#404、#407、#408），9 个间接影响 API/运维，其余为 provider 请求、功能建议或信息不足的支持问题。

## 全部 23 个 open PR

| # | Base | +/− | Files | Apply | Verdict | Reason / evidence |
|---:|---|---:|---:|---|---|---|
| [82](https://github.com/aimod-cc/Agent2API/pull/82) | main | +632/−0 | 9 | CONFLICT | **Obsolete** | Adds in-tree `core/providers/lobster/*`; beta delegates LobsterAI to plugin (`missing_plugin.rs:170`) and has `lobsterai_accounts.rs`. Wrong architecture and conflicts. |
| [83](https://github.com/aimod-cc/Agent2API/pull/83) | main | +3,206/−12 | 21 | CONFLICT | **Obsolete** | Part 2/2 of #82; same plugin-architecture problem; cannot stand alone. |
| [109](https://github.com/aimod-cc/Agent2API/pull/109) | main | +613/−40 | 4 | CONFLICT | **Obsolete** | Touches `core/providers/trae/*`; Trae is plugin-delegated (`missing_plugin.rs:157`); files absent in beta. |
| [199](https://github.com/aimod-cc/Agent2API/pull/199) | beta | +1,750/−33 | 12 | CONFLICT | **Port-worthy, high risk** | Directly addresses #408/image routing, but includes a 997-line new `image_strip.rs` and old model/provider paths; split capability filter from image-strip. |
| [252](https://github.com/aimod-cc/Agent2API/pull/252) | main | +112,136/−6,036 | 693 | EMPTY* | **Reject** | 99-commit personal `dev` branch; huge unrelated diff; not a reviewable feature PR. |
| [259](https://github.com/aimod-cc/Agent2API/pull/259) | beta | +85/−0 | 1 | CONFLICT | **Obsolete** | `core/providers/officeace/chat.rs` absent; OfficeAce is plugin-delegated (`missing_plugin.rs:164`). |
| [260](https://github.com/aimod-cc/Agent2API/pull/260) | beta | +237/−7 | 3 | CONFLICT | **Port-worthy** | Loomy Chinese size-wall classification is still absent in beta.6; port the two logic hunks + tests. |
| [278](https://github.com/aimod-cc/Agent2API/pull/278) | main | +111/−39 | 9 | CONFLICT | **Partial port** | Default report refresh still 1 s; `idx_requests_summary` already exists at `schema.rs:822`; old schema-v14 hunk must be dropped. |
| [294](https://github.com/aimod-cc/Agent2API/pull/294) | beta | +224/−5 | 2 | CONFLICT | **Obsolete** | Trae `forward.rs`/`stream.rs` absent; plugin-delegated. |
| [298](https://github.com/aimod-cc/Agent2API/pull/298) | beta | +166/−1 | 4 | CONFLICT | **Obsolete** | `core/protocol/antigravity_*` absent; Antigravity is plugin-delegated (`missing_plugin.rs:163`). |
| [305](https://github.com/aimod-cc/Agent2API/pull/305) | main | +1/−1 | 1 | **CLEAN** | **Merge-worthy** | Fixes SenseNova preset base URL to `https://token.sensenova.cn/v1`; beta still has old URL at `ui/preset-providers.js:284`. |
| [310](https://github.com/aimod-cc/Agent2API/pull/310) | beta | +374/−42 | 14 | CONFLICT | **Mostly duplicate** | Beta already has `customList`, `customRecordsFresh`, sparse capabilities and `setCapabilities` custom branch in `assistant-tools.ts`; remaining i18n/responsive changes are cosmetic. |
| [311](https://github.com/aimod-cc/Agent2API/pull/311) | main | +58/−0 | 1 | **CLEAN** | **Merge-worthy** | Adds `.github/workflows/server-tests.yml` running `cargo test -p agent2api-server`; no such workflow exists. |
| [312](https://github.com/aimod-cc/Agent2API/pull/312) | beta | +9,392/−56 | 79 | CONFLICT | **Reject / split** | AtomCode + LobsterAI + i18n + billing in one huge PR; beta already has `atomcode_accounts.rs`; provider is plugin-delegated. Not deployment-critical. |
| [317](https://github.com/aimod-cc/Agent2API/pull/317) | beta | +145/−6 | 3 | CONFLICT | **Port to plugin** | Real OrcaTerm bugs: `tokenExpireAt` ms treated as seconds and HTTP-200 business-code credential rejection; `core/providers/orcaterm/*` absent in beta. |
| [321](https://github.com/aimod-cc/Agent2API/pull/321) | main | +88,705/−5,813 | 653 | EMPTY* | **Reject** | 653-file personal branch for account groups; not reviewable and overlaps #196. |
| [336](https://github.com/aimod-cc/Agent2API/pull/336) | main | +87/−12 | 3 | MERGEABLE | **Split** | Anthropic `message_delta` input-usage fix is applicable; `qoder/protocol.rs` is absent in beta and must be plugin-side. |
| [337](https://github.com/aimod-cc/Agent2API/pull/337) | main | +24/−2 | 1 | MERGEABLE | **Obsolete** | Edits `core/models/mod.rs::builtin_models()`, which no longer exists in beta (model catalog moved). |
| [343](https://github.com/aimod-cc/Agent2API/pull/343) | beta | +121/−0 | 2 | **CLEAN** | **Out of scope** | Adds only `scripts/repro/zcode-start-plan-3012` docs/JSON/patch; no runtime fix. |
| [384](https://github.com/aimod-cc/Agent2API/pull/384) | beta | +402/−71 | 14 | CONFLICT | **Out of scope** | Expiring-credit UI only; not deployment/API. |
| [415](https://github.com/aimod-cc/Agent2API/pull/415) | main | +55/−0 | 2 | CONFLICT | **Already integrated** | Beta `ui/index.html:106` references `css/mobile.css` and line 133 references `mobile-columns.js`; duplicate. |
| [417](https://github.com/aimod-cc/Agent2API/pull/417) | beta | +476/−95 | 14 | **CLEAN** | **Optional feature** | Qoder web-cookie UI/API/bridge; depends on plugin `webcookie.info/set/clear`; review security and plugin-version gating. Not needed for local deploy. |
| [418](https://github.com/aimod-cc/Agent2API/pull/418) | beta | +393/−124 | 21 | **CLEAN** | **Out of scope** | Two warm themes + settings entry; explicitly UI-theme-only, no deployment value. |

\* #252 and #321 return `HTTP 406: diff exceeded the maximum number of files (300)` / `PullRequest.diff too_large`; file lists and commit counts were read from the API instead. They are **not** empty PRs; diffs are simply unavailable through `gh pr diff`.

### PR 处理结论

- **已整合并验证：** #305、#311。
- **已存在/已过时，不重复合并：** #415（beta 的 `index.html` 已引用 `mobile.css`/`mobile-columns.js`）、#337（`builtin_models()` 已不存在）、#259（OfficeAce 已插件化）、#294/#298/#109/#82/#83/#317（对应 provider 已迁到插件或文件在 beta 不存在）。
- **可移植但本次未做：** #260（Loomy 中文长度墙）、#278（报表默认 1s 刷新）、#199（视觉能力路由，和 #408 重叠）、#336（Anthropic usage 部分）。
- **拒绝：** #252（693 文件、+112k 行个人分支）、#321（653 文件、+88k 行账号组分支）、#312（79 文件、+9.4k 行，混合多个功能）。
- **仅文档/主题/可选功能：** #343、#417、#418。

## 相关已关闭 issue / PR（部署与 API 相关）

| 编号 | 状态 | 结论 |
| --- | --- | --- |
| [#391](https://github.com/aimod-cc/Agent2API/issues/391) | 已关闭，已修 | Docker beta.5 更新时 `tokio::spawn` 无 runtime 的 panic；beta.5.1 `83c4a37` 用 `crate::spawn_task` 修复，beta.6 代码中存在。 |
| [#401](https://github.com/aimod-cc/Agent2API/issues/401) | 已关闭，未证实 | Linux amd64 + 旧 `user_version=20` 启动 exit 134；没有对应提交，beta.6 迁移路径仍在。本次未复现，标为未验证风险。 |
| [#403](https://github.com/aimod-cc/Agent2API/issues/403) | 已关闭，部分缓解 | 登录循环被 `?from=login` + token-in-body 兜底缓解；`cookie_probe` 只返回一个 token，而登录页要两个，契约仍不一致。 |
| [#375](https://github.com/aimod-cc/Agent2API/issues/375) | 已关闭，代码仍在 | 桌面窗口 `tauri://localhost`，`login.html` 仍用相对 `fetch('/api/panel/status')`；本次未构建原生桌面，不能声称已修。 |
| [#359](https://github.com/aimod-cc/Agent2API/issues/359) | 已关闭，部分 | UMID 的 “一键安装” UI 已存在；实际安装目录/持久化在外部插件，需插件侧验证。 |
| [#46](https://github.com/aimod-cc/Agent2API/pull/46) | 已关闭，部分 cherry-pick | 远程面板回调能力以 `a5dc9c5`/`cc18c22`/`57c7523` 等提交进入 beta，不是整 PR 合并。 |
| [#123](https://github.com/aimod-cc/Agent2API/pull/123) | 已合并 | 桌面桥 `getCors`/`saveCors`，beta 与 web_shim 都在。 |
| [#145](https://github.com/aimod-cc/Agent2API/pull/145) | 已合并 | `&str` 字节切片 panic 修复，merge `7d1580f`；对 `panic=abort` 的进程生命周期重要。 |
| [#102](https://github.com/aimod-cc/Agent2API/pull/102) | 已关闭，部分 | `data:[DONE]` 无空格修复被 cherry-pick 为 `9b24e4f`；CodeArts max-token clamp 未合并。 |
| [#86](https://github.com/aimod-cc/Agent2API/pull/86) | 已关闭，本地合并 | 拒绝空 Anthropic 流，merge `f0279c2`。 |

## 本分支实际整合记录

| PR | 变更 | 验证 |
| --- | --- | --- |
| #305 | `desktop-tauri/ui/preset-providers.js` 的 SenseNova base URL 改为 `https://token.sensenova.cn/v1` | `git apply` 成功；文件行号检查通过；不改变路由/凭据逻辑。 |
| #311 | 新增 `.github/workflows/server-tests.yml`，在 main/beta 的 push/PR 跑 `cargo test -p agent2api-server` | 工作流文件检查；本地用同一命令跑测试（结果见 `OMARCHY_LINUX.md` / 最终报告）。 |

> 说明：本分支还修复了 beta.6 中两个**测试目标无法编译**的陈旧初始化（`RequestQuery` 缺字段、`send_with_retry` 参数变更）。不修这两处，CI 工作流会一启动就编译失败；这是 #311 能真正生效的前提。
