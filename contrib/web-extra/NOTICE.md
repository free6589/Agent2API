# 第三方来源与修改声明（NOTICE）

本包是 `Agent2API` 的贡献组件，整体按仓库根 `LICENSE`（GNU Affero General Public
License v3.0，AGPL-3.0）分发。本文件声明第三方来源与修改情况。

## 协议行为参考来源

`crates/providers/src/providers/qwen/`（通义千问网页版适配）的**协议行为**
（接口路径、请求体字段、SSE 事件形态、多候选与风控表现）整理自当时公开的
开源实现，并在本仓库按 AGPL-3.0 重新实现：

- 主要参考：`cv-superding/dsweb-proxy`（`providers/qwen.ts`），**Apache License
  2.0**。Apache-2.0 代码可纳入 AGPL-3.0 项目分发；按其 §4(a)(b) 要求，本文件
  即为来源声明与「已修改」标注：本包的实现针对 Agent2API v3.4.0 插件 SDK
  （`agent2api-provider-protocol` / `agent2api-provider-sdk`，tag `v3.4.0`）重写，
  模块结构、trait 形状、账号代理处理与登录申报方式均与上游 SDK 规范一致，
  与参考文件不存在逐行对应关系。
- 参考来源的模块头自述其行为「逐行核对过 `Rfym21/Qwen2API`」。该仓库 README
  声明「仅限个人学习与研究 / 禁止商用」、不是 OSI 许可证。**本包没有复制该
  仓库的任何代码**；如果这一点不成立或你不接受该仓库的使用条款，请勿分发或
  使用本模块，并向上游（本 Fork 维护者）反馈。

Apache License 2.0 全文：<https://www.apache.org/licenses/LICENSE-2.0>

## 其它

- 本包其余代码（SDK 对接、提示词折叠与 `<tool_call>` 模拟约定、SSE 解码骨架）
  与官方网页插件包 `Agent2WebProviders`（AGPL-3.0）同源同风格，均为 AGPL-3.0
  仓库内代码，无额外第三方来源。
- 商标与品牌：`chat.qwen.ai`、通义千问及相关标识是各自权利人的商标；本包的
  使用不构成商标授权。
