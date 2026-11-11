//! 转发：把一次 OpenAI chat 请求送到千帆（V2 直传 / V1 翻译），流式回推。
//!
//! ── 恒流式约定 ─────────────────────────────────────────────
//! 网关对有状态家的契约是「插件恒按 stream 推 SSE，客户端要非流式时网关
//! 自己聚合」（`plugin_host::rpc` 转发路径的注释原文）。因此这里只写一条
//! 流式路径：上游侧 V1/V2 都以 `stream=true` 发出，产物统一翻成 OpenAI
//! chat.completion.chunk 帧 + usage 帧 + `data: [DONE]`。
//!
//! ── 错误怎么过线 ───────────────────────────────────────────
//!   · head 之前失败（账号缺失、凭证解析失败、token 换取失败）：
//!     `Err(control_error(status, …))` —— SDK 还原成带状态码的 WireError，
//!     网关按控制面错误处理。
//!   · 上游拒绝（HTTP ≥ 400，或 V1 的 HTTP 200 + 错误帧）：`head(状态码,
//!     application/json)` + 原样上游 JSON，然后正常返回 —— 网关会把 body
//!     收齐后走本插件的 `classify` 做四档分类（TokenExpired / QuotaLimited /
//!     ContentBlocked / Fatal），编排层据此决定刷新重试或换账号。
//!   · head 之后失败（流中断、翻译不了）：`Err(…)`，网关按流中断处理。
//!
//! ── 硬约束 ────────────────────────────────────────────────
//! SDK 侧 `deny(clippy::unwrap_used, …)`：本文件零 unwrap/expect/panic；
//! 任何一步拿不到就返回可读错误，不猜。

use agent2api_provider_protocol as proto;
use agent2api_provider_sdk::ctx::PluginCtx;
use agent2api_provider_sdk::{control_error, ConversationSink};
use futures::StreamExt;
use serde_json::{json, Value};

use super::credentials::{self, Credential};
use super::models;
use crate::http;

/// 发一条 OpenAI chunk（delta + finish）。
async fn send_delta(
    sink: &mut ConversationSink<'_>,
    id: &str,
    model: &str,
    delta: Value,
    finish: Option<&str>,
) -> Result<(), String> {
    let frame = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": http::now_ms() / 1000,
        "model": model,
        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
    });
    sink.send(format!("data: {frame}\n\n").as_bytes()).await
}

/// 发一个 usage-only 帧（`choices: []`，聚合器从这里读真值 usage）。
async fn send_usage(
    sink: &mut ConversationSink<'_>,
    id: &str,
    model: &str,
    usage: &Value,
) -> Result<(), String> {
    let frame = json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": http::now_ms() / 1000,
        "model": model,
        "choices": [],
        "usage": usage,
    });
    sink.send(format!("data: {frame}\n\n").as_bytes()).await
}

async fn send_done(sink: &mut ConversationSink<'_>) -> Result<(), String> {
    sink.send(b"data: [DONE]\n\n").await
}

/// 上游错误：head(状态码) + 原 JSON，交网关分类。**head 之前**才允许走这里。
async fn send_upstream_error(
    sink: &mut ConversationSink<'_>,
    status: u16,
    body: &Value,
) -> Result<(), String> {
    sink.head(proto::StreamHead {
        status,
        headers: [("content-type".to_string(), "application/json".to_string())].into_iter().collect(),
        upstream: proto::UpstreamKind::Chat,
    })
    .await?;
    sink.send(body.to_string().as_bytes()).await
}

/// 客户端要发的模型名（界面回显用）与上游发送名分开取。
fn requested_model(req: &proto::ConversationRequest) -> String {
    if !req.wire_model.trim().is_empty() {
        return req.wire_model.trim().to_string();
    }
    req.body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("")
        .to_string()
}

/// content 字段的文本形态：字符串原样；数组拼 text 部分。
/// V1 旧端点没有视觉能力，遇到图片/文件分片直接报错（API Key 模式无此限制）。
fn content_text(content: &Value, allow_multimodal_hint: &str) -> Result<String, String> {
    match content {
        Value::String(text) => Ok(text.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            let mut saw_text = false;
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                    saw_text = true;
                } else if part.get("type").and_then(Value::as_str).is_some() {
                    return Err(format!(
                        "旧版 AK/SK 端点不支持多模态输入（{allow_multimodal_hint}）；\
                         请改用 API Key 模式并选择支持视觉的模型"
                    ));
                }
            }
            if saw_text {
                Ok(out)
            } else {
                Err("message.content 是空的多模态数组，无法转成旧版端点的文本".to_string())
            }
        }
        Value::Null => Ok(String::new()),
        other => Err(format!(
            "message.content 类型不支持（{}）",
            match other {
                Value::Object(_) => "object",
                _ => "非常规形态",
            }
        )),
    }
}

/// V1 旧端点请求体：OpenAI messages → 百度 messages + system 分离。
///
/// 判据：旧版 wenxinworkshop 对话接口的人设是顶层 `system` 字段（不在
/// messages 里），输出上限字段叫 `max_output_tokens`，不支持 function
/// calling / tool 消息 / stream_options —— 全部如实拒绝或丢弃，不猜。
fn v1_body(source: &Value, send_id: &str) -> Result<Value, String> {
    let empty = Vec::new();
    let incoming = source
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    if incoming.is_empty() {
        return Err("请求缺少 messages".to_string());
    }
    let mut messages = Vec::new();
    let mut system_parts: Vec<String> = Vec::new();
    for message in incoming {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        match role {
            "system" => {
                system_parts.push(content_text(
                    message.get("content").unwrap_or(&Value::Null),
                    "system 分片",
                )?);
            }
            "user" | "assistant" => {
                if message.get("tool_calls").is_some() {
                    return Err(
                        "旧版 AK/SK 端点不支持 function calling：请改用 API Key 模式（千帆 V2）\
                         ，或去掉 tools 后重试"
                            .to_string(),
                    );
                }
                messages.push(json!({
                    "role": role,
                    "content": content_text(message.get("content").unwrap_or(&Value::Null), role)?,
                }));
            }
            "tool" | "function" => {
                return Err(
                    "旧版 AK/SK 端点不支持 tool/function 消息：请改用 API Key 模式（千帆 V2）"
                        .to_string(),
                );
            }
            other => return Err(format!("消息角色不认识：{other}")),
        }
    }
    if messages.is_empty() {
        return Err("messages 里没有可发送的 user/assistant 内容".to_string());
    }
    let mut out = json!({
        "model": send_id,
        "messages": messages,
        "stream": true,
    });
    if !system_parts.is_empty() {
        out["system"] = json!(system_parts.join("\n\n"));
    }
    for (from, to) in [
        ("max_tokens", "max_output_tokens"),
        ("max_completion_tokens", "max_output_tokens"),
    ] {
        if let Some(value) = source.get(from).filter(|value| value.is_i64()) {
            out[to] = value.clone();
            break;
        }
    }
    for key in ["temperature", "top_p", "stop", "user"] {
        if let Some(value) = source.get(key) {
            if !value.is_null() {
                out[key] = value.clone();
            }
        }
    }
    Ok(out)
}

/// V2 请求体：OpenAI 兼容直传 + 三处必改（model / stream / include_usage）。
///
/// `include_usage` 必须注入：千帆流式默认**不回** usage（官方文本生成文档
/// stream_options 节），聚合器读不到真值会让非流式请求的 usage 归零 ——
/// 与 ModelScope 插件「静默毁账」同款的坑，这里是官方给出的开关，直接打开。
/// 客户端已写 `stream_options` 时只置 `include_usage`，其余键保留。
fn v2_body(source: &Value, send_id: &str, thinking: Option<bool>) -> Value {
    let mut body = source.clone();
    if let Some(object) = body.as_object_mut() {
        object.insert("model".to_string(), json!(send_id));
        object.insert("stream".to_string(), json!(true));
        let mut options = object
            .get("stream_options")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        options.insert("include_usage".to_string(), Value::Bool(true));
        object.insert("stream_options".to_string(), Value::Object(options));
        // 思考开关：客户端/网关已显式表达（thinking / enable_thinking /
        // reasoning_effort）时一字不动；只有后缀别名解开来的意图才注入。
        let has_explicit = ["thinking", "enable_thinking", "reasoning_effort"]
            .iter()
            .any(|key| object.contains_key(*key));
        if let Some(want) = thinking {
            if !has_explicit {
                object.insert(
                    "thinking".to_string(),
                    json!({ "type": if want { "enabled" } else { "disabled" } }),
                );
            }
        }
    }
    body
}

/// 旧版错误码 → 客户端可见的 HTTP 状态（分类判据在 `classify`，用 error_code）。
fn v1_error_status(code: i64) -> u16 {
    match code {
        110 | 111 => 401,
        2 | 4 | 17 | 18 | 19 => 429,
        6 | 13 => 403,
        14 => 429,
        100 | 336_100..=336_199 => 400,
        _ => 502,
    }
}

/// 旧版 finish_reason → OpenAI 的。
fn v1_finish(reason: &str) -> Option<&'static str> {
    match reason {
        "normal" => Some("stop"),
        "length" => Some("length"),
        "content_filter" => Some("content_filter"),
        _ => None,
    }
}

/// 取一枚**现在能用**的旧版 access_token：缓存新鲜直接用；否则换取并回写。
async fn ensure_v1_token(
    ctx: &PluginCtx,
    record: &agent2api_provider_sdk::ctx::StoreAccount,
    cred: &Credential,
    proxy: Option<&proto::WireProxy>,
) -> Result<String, String> {
    if let Some((token, _)) = credentials::cached_v1_token(&record.secret) {
        return Ok(token);
    }
    let (token, expires_at) = credentials::fetch_v1_token(proxy, &ctx.env.proxy, cred).await?;
    // 回写失败不拦路：token 已到手，这次请求照常走；下一条请求再试写回。
    let _ = credentials::save_refreshed_token(ctx, record, &token, expires_at).await;
    Ok(token)
}

/// 主入口：一次会话式转发。
pub async fn forward(
    ctx: &PluginCtx,
    req: proto::ConversationRequest,
    sink: &mut ConversationSink<'_>,
) -> Result<(), String> {
    let record = ctx
        .store_get("wenxin", &req.account)
        .await
        .map_err(|error| match error {
            agent2api_provider_sdk::StoreError::NotFound => {
                control_error(404, "文心账号不存在（请先在「添加账号」里粘贴千帆凭证）", None, None)
            }
            other => control_error(502, format!("读取文心账号失败：{other}"), None, None),
        })?;
    let cred = credentials::parse(&record.secret).map_err(|message| control_error(400, message, None, None))?;
    let requested = requested_model(&req);
    if requested.is_empty() {
        return Err(control_error(400, "请求缺少 model", None, None));
    }
    let (send_id, thinking) = models::resolve_model(&requested);
    if send_id.is_empty() {
        return Err(control_error(400, "请求的模型名为空", None, None));
    }

    let (url, headers, body) = if cred.is_ak_sk() {
        let token = ensure_v1_token(ctx, &record, &cred, req.proxy.as_ref()).await?;
        (
            format!("{}{}?access_token={}", cred.chat_base, "", token),
            vec![("Accept".to_string(), "text/event-stream".to_string())],
            v1_body(&req.body, &send_id)?,
        )
    } else {
        (
            format!("{}/chat/completions", cred.chat_base),
            vec![
                ("Authorization".to_string(), format!("Bearer {}", cred.api_key)),
                ("Accept".to_string(), "text/event-stream".to_string()),
            ],
            v2_body(&req.body, &send_id, thinking),
        )
    };

    let client = http::client(req.proxy.as_ref(), &ctx.env.proxy);
    let mut builder = client
        .post(&url)
        .timeout(http::CHAT_TIMEOUT)
        .header("Content-Type", "application/json");
    for (name, value) in &headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let response = builder
        .body(body.to_string())
        .send()
        .await
        .map_err(|error| format!("文心上游连接失败：{error}"))?;
    let status = response.status().as_u16();

    // 上游拒绝：把 body 收齐原样交出去，网关按 classify 分类。
    if status >= 400 {
        let text = response
            .text()
            .await
            .map_err(|error| format!("读取文心错误响应失败：{error}"))?;
        let parsed: Value = serde_json::from_str(&text).unwrap_or_else(|_| {
            json!({ "message": if text.is_empty() { format!("上游返回 {status}") } else { text.clone() } })
        });
        return send_upstream_error(sink, status, &parsed).await;
    }

    let is_sse = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|mime| mime.contains("text/event-stream"));
    if !is_sse {
        // 上游没按流式回（旧端点偶发忽略 stream 参数）：整体读完按单帧翻。
        let text = response
            .text()
            .await
            .map_err(|error| format!("读取文心响应失败：{error}"))?;
        let payload: Value = serde_json::from_str(&text)
            .map_err(|_| format!("文心响应既不是 SSE 也不是 JSON（HTTP {status}）"))?;
        return emit_whole_response(sink, &requested, &payload).await;
    }

    if cred.is_ak_sk() {
        stream_v1(sink, response, &requested).await
    } else {
        stream_v2(sink, response, &requested).await
    }
}

/// 旧版（V1）SSE → OpenAI chunk。
///
/// ── 为什么要先偷看第一帧 ────────────────────────────────────
/// 旧端点的业务错误走 **HTTP 200 + 错误 JSON**（`{"error_code": …}`），
/// 包括流式请求 —— 不偷看第一帧就会把错误当正文发给客户端（「HTTP 200
/// 可能是失败」这条已知坑）。第一帧是正常数据才发 200 的 head。
async fn stream_v1(
    sink: &mut ConversationSink<'_>,
    response: reqwest::Response,
    model: &str,
) -> Result<(), String> {
    let response_id = format!("chatcmpl-wx{}", http::now_ms());
    let mut buffer = Vec::<u8>::new();
    let mut role_sent = false;
    let mut head_sent = false;
    let mut usage: Option<Value> = None;
    let mut source = response.bytes_stream();

    macro_rules! head_ok {
        ($sink:expr) => {
            if !head_sent {
                $sink
                    .head(proto::StreamHead {
                        status: 200,
                        headers: [("content-type".to_string(), "text/event-stream".to_string())].into_iter().collect(),
                        upstream: proto::UpstreamKind::Chat,
                    })
                    .await?;
                head_sent = true;
            }
        };
    }

    while let Some(chunk) = source.next().await {
        let bytes = chunk.map_err(|error| format!("文心流式传输中断：{error}"))?;
        buffer.extend_from_slice(&bytes);
        while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=position).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]).trim().to_string();
            if text.is_empty() {
                continue;
            }
            let Some(payload) = text.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload.is_empty() {
                continue;
            }
            let frame: Value = match serde_json::from_str(payload) {
                Ok(frame) => frame,
                Err(_) => continue, // 非 JSON 行（心跳等）忽略
            };
            // 错误帧：head 还没发 → 走错误路径；已发 → 只能如实中断。
            if frame.get("error_code").is_some() {
                let code = frame.get("error_code").and_then(Value::as_i64).unwrap_or(0);
                let status = v1_error_status(code);
                if !head_sent {
                    return send_upstream_error(sink, status, &frame).await;
                }
                return Err(format!(
                    "文心流中途返回错误 {}: {}",
                    code,
                    frame.get("error_msg").and_then(Value::as_str).unwrap_or("")
                ));
            }
            head_ok!(sink);
            let result = frame.get("result").and_then(Value::as_str).unwrap_or("");
            if !result.is_empty() {
                let delta = if role_sent {
                    json!({ "content": result })
                } else {
                    role_sent = true;
                    json!({ "role": "assistant", "content": result })
                };
                send_delta(sink, &response_id, model, delta, None).await?;
            }
            if frame.get("usage").map(Value::is_object).unwrap_or(false) {
                usage = frame.get("usage").cloned();
            }
            let need_clear = frame
                .get("need_clear_history")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let finish = if need_clear {
                Some("content_filter")
            } else {
                frame
                    .get("finish_reason")
                    .and_then(Value::as_str)
                    .and_then(v1_finish)
            };
            if frame.get("is_end").and_then(Value::as_bool).unwrap_or(false) {
                let finish = finish.unwrap_or("stop");
                send_delta(sink, &response_id, model, json!({}), Some(finish)).await?;
                if let Some(usage) = usage {
                    send_usage(sink, &response_id, model, &usage).await?;
                }
                return send_done(sink).await;
            }
        }
    }
    if !head_sent {
        return Err("文心流式响应为空（没有可解析的帧）".to_string());
    }
    // 上游没发 is_end 就断了：按 stop 收尾，聚合器至少能拼出完整正文。
    send_delta(sink, &response_id, model, json!({}), Some("stop")).await?;
    if let Some(usage) = usage {
        send_usage(sink, &response_id, model, &usage).await?;
    }
    send_done(sink).await
}

/// V2 SSE：帧本身就是 OpenAI 形态，只改写 model 名后逐帧透传。
async fn stream_v2(
    sink: &mut ConversationSink<'_>,
    response: reqwest::Response,
    model: &str,
) -> Result<(), String> {
    sink.head(proto::StreamHead {
        status: 200,
        headers: [("content-type".to_string(), "text/event-stream".to_string())].into_iter().collect(),
        upstream: proto::UpstreamKind::Chat,
    })
    .await?;
    let mut buffer = Vec::<u8>::new();
    let mut source = response.bytes_stream();
    while let Some(chunk) = source.next().await {
        let bytes = chunk.map_err(|error| format!("文心流式传输中断：{error}"))?;
        buffer.extend_from_slice(&bytes);
        while let Some(position) = buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = buffer.drain(..=position).collect();
            let text = String::from_utf8_lossy(&line[..line.len() - 1]).trim().to_string();
            if text.is_empty() {
                continue;
            }
            let Some(payload) = text.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload == "[DONE]" {
                return send_done(sink).await;
            }
            if payload.is_empty() {
                continue;
            }
            let mut frame: Value = match serde_json::from_str(payload) {
                Ok(frame) => frame,
                Err(_) => continue,
            };
            if let Some(object) = frame.as_object_mut() {
                object.insert("model".to_string(), json!(model));
            }
            sink.send(format!("data: {frame}\n\n").as_bytes()).await?;
        }
    }
    // 上游没给 [DONE] 就断流：补一个收尾帧，别让客户端挂着等。
    send_delta(sink, "chatcmpl-wx", model, json!({}), Some("stop")).await?;
    send_done(sink).await
}

/// 非流式响应（V1 整包 JSON，或 V2 罕见的非 SSE 回包）→ 单帧流。
async fn emit_whole_response(
    sink: &mut ConversationSink<'_>,
    model: &str,
    payload: &Value,
) -> Result<(), String> {
    // V1 错误信封（HTTP 200 + error_code）
    if payload.get("error_code").is_some() {
        let code = payload.get("error_code").and_then(Value::as_i64).unwrap_or(0);
        return send_upstream_error(sink, v1_error_status(code), payload).await;
    }
    sink.head(proto::StreamHead {
        status: 200,
        headers: [("content-type".to_string(), "text/event-stream".to_string())].into_iter().collect(),
        upstream: proto::UpstreamKind::Chat,
    })
    .await?;
    let response_id = format!("chatcmpl-wx{}", http::now_ms());
    // V1 整包：{result, is_truncated, usage, finish_reason}
    if let Some(result) = payload.get("result").and_then(Value::as_str) {
        let need_clear = payload
            .get("need_clear_history")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let finish = if need_clear {
            "content_filter"
        } else {
            payload
                .get("finish_reason")
                .and_then(Value::as_str)
                .and_then(v1_finish)
                .unwrap_or("stop")
        };
        send_delta(
            sink,
            &response_id,
            model,
            json!({ "role": "assistant", "content": result }),
            None,
        )
        .await?;
        send_delta(sink, &response_id, model, json!({}), Some(finish)).await?;
        if let Some(usage) = payload.get("usage").filter(|value| value.is_object()) {
            send_usage(sink, &response_id, model, usage).await?;
        }
        return send_done(sink).await;
    }
    // V2 整包：{choices:[{message:{...}, finish_reason}], usage}
    let choice = payload.pointer("/choices/0");
    let message = choice
        .and_then(|item| item.get("message"))
        .cloned()
        .unwrap_or_else(|| json!({ "role": "assistant", "content": "" }));
    let finish = choice
        .and_then(|item| item.get("finish_reason"))
        .and_then(Value::as_str)
        .unwrap_or("stop");
    let mut delta = message.clone();
    if let Some(object) = delta.as_object_mut() {
        object.remove("role");
    }
    send_delta(
        sink,
        payload.get("id").and_then(Value::as_str).unwrap_or(&response_id),
        model,
        delta,
        None,
    )
    .await?;
    send_delta(sink, &response_id, model, json!({}), Some(finish)).await?;
    if let Some(usage) = payload.get("usage").filter(|value| value.is_object()) {
        send_usage(sink, &response_id, model, usage).await?;
    }
    send_done(sink).await
}
