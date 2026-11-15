//! 在公共 Chat 出口补齐用量字段，覆盖普通上游与插件流；其他事件保留原始字节。

use std::collections::VecDeque;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use serde_json::Value;

use crate::server::core::protocol::usage::{normalize_chat, TokenUsage};

use super::usage::RequestTelemetry;
use super::ForwardOutcome;

type Source = Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send + Unpin>;

pub(super) fn normalize_outcome(outcome: ForwardOutcome, telemetry: Arc<RequestTelemetry>) -> ForwardOutcome {
    match outcome {
        ForwardOutcome::Completion { mut body } => {
            if let Some(usage) = body.get_mut("usage").filter(|usage| usage.is_object()) {
                normalize_chat(usage);
                telemetry.report_usage(usage);
            }
            ForwardOutcome::Completion { body }
        }
        ForwardOutcome::Stream { status, stream } => ForwardOutcome::Stream {
            status,
            stream: Box::new(UsageStream {
                source: stream,
                telemetry,
                tokens: TokenUsage::default(),
                line: Vec::new(),
                pending: VecDeque::new(),
                passthrough_line: false,
                ended: false,
                tool_state: super::tool_delta::StreamToolCalls::new(),
            }),
        },
    }
}

struct UsageStream {
    source: Source,
    telemetry: Arc<RequestTelemetry>,
    tokens: TokenUsage,
    line: Vec<u8>,
    pending: VecDeque<Result<Bytes, io::Error>>,
    passthrough_line: bool,
    ended: bool,
    /// 流式 tool_calls 的逐 index 聚合状态（幽灵丢弃 + 终帧修正，见 [`super::tool_delta`] 模块头）。
    ///
    /// 状态必须跨行持续整个流：具名分片可能在幽灵分片之前几十帧就出现过。
    tool_state: super::tool_delta::StreamToolCalls,
}

impl UsageStream {
    const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

    fn feed(&mut self, bytes: &[u8]) {
        let mut rest = bytes;
        while !rest.is_empty() {
            let newline = rest.iter().position(|byte| *byte == b'\n');
            let end = newline.map_or(rest.len(), |index| index + 1);
            let part = &rest[..end];
            rest = &rest[end..];
            if self.passthrough_line {
                self.pending.push_back(Ok(Bytes::copy_from_slice(part)));
                self.passthrough_line = newline.is_none();
                continue;
            }
            if self.line.len().saturating_add(part.len()) > Self::MAX_LINE_BYTES {
                self.flush_raw();
                self.pending.push_back(Ok(Bytes::copy_from_slice(part)));
                self.passthrough_line = newline.is_none();
                continue;
            }
            self.line.extend_from_slice(part);
            if newline.is_some() {
                self.flush_line();
            }
        }
        // 同一来源分片的完整行一起下发，避免把原有 SSE 帧拆成多次交付。
        if self.pending.len() > 1 {
            let mut combined = Vec::new();
            while let Some(Ok(bytes)) = self.pending.pop_front() {
                combined.extend_from_slice(&bytes);
            }
            self.pending.push_back(Ok(Bytes::from(combined)));
        }
    }

    fn flush_raw(&mut self) {
        if !self.line.is_empty() {
            self.pending.push_back(Ok(Bytes::from(std::mem::take(&mut self.line))));
        }
    }

    fn flush_line(&mut self) {
        if self.line.is_empty() { return; }
        let line = std::mem::take(&mut self.line);
        let rewritten = self.rewrite_line(&line);
        self.pending.push_back(Ok(rewritten.unwrap_or_else(|| Bytes::from(line))));
    }

    /// 行级改写三件套（有一件命中才重序列化，否则返回 `None`保持原字节）：
    ///
    /// 1. usage 补齐（既有行为，保持“上报与改写与否无关”的旧序）；
    /// 2. 空 `finish_reason ""` → `null`（对照 shim `EMPTY_FINISH_RE`，任意空格形态）
    ///    —— OpenAI 规范里流式中间帧的结束理由是 null，空串不是非终止信号；
    /// 3. 幽灵 `tool_calls` 分片丢弃 + 终帧 `tool_calls` → `stop` 修正
    ///    （对照 shim v2.15，判据见 [`super::tool_delta`] 模块头）。
    fn rewrite_line(&mut self, line: &[u8]) -> Option<Bytes> {
        let payload = line.strip_prefix(b"data:")?;
        let has_usage = payload.windows(7).any(|window| window == b"\"usage\"");
        let has_tool_calls = payload.windows(12).any(|window| window == b"\"tool_calls\"");
        // `"finish_reason"` = 引号 + 13 字母 + 引号，共 15 字节。
        let has_finish = payload.windows(15).any(|window| window == b"\"finish_reason\"");
        if !has_usage && !has_tool_calls && !has_finish { return None; }
        let mut chunk: Value = serde_json::from_slice(payload).ok()?;
        // 旧序保留：上报发生在“改写与否”之前，统计少记一条可以接受，
        // 把请求搞坏不可接受——与改造前一致，未上报成功时整行保持原字节。
        let mut changed = false;
        if has_usage {
            if let Some(usage) = chunk.get_mut("usage") {
                if let Some(tokens) = TokenUsage::from_chat(usage) {
                    self.tokens.absorb(tokens);
                    changed |= self.tokens.write_chat(usage);
                    self.telemetry.report_usage(usage);
                }
            }
        }
        if has_tool_calls {
            if let Some(choices) = chunk.get_mut("choices").and_then(Value::as_array_mut) {
                for choice in choices.iter_mut() {
                    let Some(choice) = choice.as_object_mut() else { continue; };
                    if let Some(delta) = choice.get_mut("delta").and_then(Value::as_object_mut) {
                        changed |= self.tool_state.push(delta);
                    }
                    changed |= self.tool_state.finish_reason(choice);
                }
            }
        }
        if has_finish {
            changed |= super::tool_delta::rewrite_empty_finish_reason(&mut chunk);
        }
        if !changed { return None; }
        let text = serde_json::to_string(&chunk).ok()?;
        let ending = if line.ends_with(b"\r\n") { "\r\n" }
            else if line.ends_with(b"\n") { "\n" } else { "" };
        Some(Bytes::from(format!("data: {text}{ending}")))
    }
}

impl Stream for UsageStream {
    type Item = Result<Bytes, io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.as_mut().get_mut();
        loop {
            if let Some(item) = this.pending.pop_front() {
                return Poll::Ready(Some(item));
            }
            if this.ended { return Poll::Ready(None); }
            match Pin::new(&mut this.source).poll_next(context) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(bytes))) => {
                    // 快速路径：整个分片不带任何需要改写的键时直接下发，
                    // 保持原字节。三个判据与 `rewrite_line` 的入口条件同步：
                    // 增删改写项时两处一起改（否则快速路径会绕过新改写）。
                    if this.line.is_empty() && !this.passthrough_line && bytes.ends_with(b"\n")
                        && !bytes.windows(7).any(|window| window == b"\"usage\"")
                        && !bytes.windows(12).any(|window| window == b"\"tool_calls\"")
                        && !bytes.windows(15).any(|window| window == b"\"finish_reason\"")
                    {
                        return Poll::Ready(Some(Ok(bytes)));
                    }
                    this.feed(&bytes);
                }
                Poll::Ready(Some(Err(error))) => {
                    // 传输中断时不能把残留半帧修成成功事件。
                    this.flush_raw();
                    this.pending.push_back(Err(error));
                    this.ended = true;
                }
                Poll::Ready(None) => {
                    this.flush_line();
                    this.ended = true;
                }
            }
        }
    }
}
