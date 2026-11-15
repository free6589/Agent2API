//! 流式 / 非流式通用的工具调用增量归一（对照 server-shim v2.15 的幽灵丢弃 + v2.1 的空 finish 改写）。
//!
//! ── 为什么需要它 ──────────────────────────────────
//! 某些上游在工具调用流里混着发“幽灵”分片：`tool_calls[i].function.name` 为空、
//! `arguments` 也为空，且同一 `index` 从未出现过具名分片。客户端拿到这种分片
//! 拼不出可执行的调用（OpenAI 规范里 `name` 是必填的），而终帧
//! `finish_reason == "tool_calls"` 又告诉它“有待执行的调用” —— 客户端会挂起
//! 等待一个永远不会凑齐的参数。shim 的实测结论是：丢弃这种
//! 分片不会损害参数连续性（真调用总是先带名再带参）。
//!
//! ── 两个调用点 ────────────────────────────────────
//!   - 流式公共出口（`usage_normalize::UsageStream` + `sse::ReasoningCoalescer`、
//!     支持照进行时聚合）逐帧调 [`StreamToolCalls::push`] 与 [`StreamToolCalls::finish_reason`]。
//!   - 非流式聚合（`aggregate::into_completion`）在下发前调
//!     [`filter_completed_tool_calls`] 与 [`fix_completion_finish_reason`]。
//!
//! ── 与 `aggregate::merge_tool_call` 的关系 ───────────────
//! 聚合器仍用它自己的合并规则按 `index` 累积（id/type 覆盖、name/arguments 追加），
//! 本模块只负责“下发前过滤 + 终帧修正”：分层而不是把合并规则搬过来。

use serde_json::Value;

/// 流式 tool_calls 的逐 index 聚合器（对照 shim `tool_meta` 的两个键）。
///
/// - `named`: 哪些 index 出现过非空 `function.name`（对照 `seen["_named"]` 集合）；
/// - `dropped`: 丢过多少个幽灵分片（对照 `seen["_dropped"]` 计数）。
#[derive(Default, Debug)]
pub struct StreamToolCalls {
    named: std::collections::HashSet<i64>,
    dropped: u64,
}

impl StreamToolCalls {
    pub fn new() -> Self {
        Self::default()
    }

    /// 丢过多少个幽灵分片（终帧修正的入口条件之一）。
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// 是否见过具名分片（终帧修正的另一个入口条件）。
    pub fn has_named(&self) -> bool {
        !self.named.is_empty()
    }

    /// 对 `delta.tool_calls` 做行级过滤：幽灵分片直接丢、具名分片记录 index。
    ///
    /// 返回 `true` 表示本帧被改写过（有丢弃），调用方需要重新序列化该帧。
    /// 返回 `false` 时 `delta` 毫动未动，调用方保持原字节下发。
    ///
    /// 判据与 shim 同义（`_is_phantom_stream_tc` + `strip_phantom_stream_block`）：
    /// 空名、参数为空且同 index 无命名记录。参数为空指的是
    /// shim 的 `_tc_args_empty` —— 缺失 / null / `""` 去空后为空 / `"{}"`。
    pub fn push(&mut self, delta: &mut serde_json::Map<String, Value>) -> bool {
        let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) else {
            return false;
        };
        if calls.is_empty() {
            return false;
        }
        // 先记录本帧里的具名 index：同一帧内“先命名后幽灵”的排列也不丢命名本身。
        for call in calls {
            if let (Some(index), Some(name)) = (tool_call_index(call), tool_call_name(call)) {
                if !name.trim().is_empty() {
                    self.named.insert(index);
                }
            }
        }
        let before = calls.len();
        let kept: Vec<Value> = calls
            .iter()
            .filter(|call| !self.is_phantom(call))
            .cloned()
            .collect();
        let dropped = before.saturating_sub(kept.len());
        if dropped == 0 {
            return false;
        }
        self.dropped += dropped as u64;
        if kept.is_empty() {
            delta.remove("tool_calls");
        } else if let Some(slot) = delta.get_mut("tool_calls") {
            *slot = Value::Array(kept);
        }
        true
    }

    /// 终帧修正（对照 shim `fix_phantom_finish`）：丢过幽灵分片、从未见过
    /// 具名分片，而本帧 `finish_reason == "tool_calls"` —— 客户端会挂起等一个
    /// 永远凑不齐的参数，故改写为 `"stop"`，让文本轮完整收尾。
    ///
    /// 返回 `true` 表示改写过，调用方需要重新序列化该帧。
    pub fn finish_reason(&self, choice: &mut serde_json::Map<String, Value>) -> bool {
        if self.dropped == 0 || !self.named.is_empty() {
            return false;
        }
        if choice.get("finish_reason").and_then(Value::as_str) != Some("tool_calls") {
            return false;
        }
        choice.insert("finish_reason".to_string(), Value::String("stop".to_string()));
        true
    }

    fn is_phantom(&self, call: &Value) -> bool {
        if tool_call_name(call).is_some_and(|name| !name.trim().is_empty()) {
            return false;
        }
        if tool_call_index(call).is_some_and(|index| self.named.contains(&index)) {
            return false;
        }
        let args_empty = call
            .get("function")
            .and_then(|function| {
                let args = function.get("arguments")?;
                if args.is_null() {
                    return Some(true);
                }
                args.as_str().map(|text| {
                    let text = text.trim();
                    text.is_empty() || text == "{}"
                })
            })
            .unwrap_or(true);
        args_empty
    }
}

/// 空 `finish_reason ""` 是否出现在本帧（对照 shim `EMPTY_FINISH_RE`：任意空格形态）。
///
/// 行级检查先走字节包含（`UsageStream::poll_next` 的快速路径保留），
/// 命中后再走 JSON 级改写：`choices[].finish_reason == ""` → `null`。
/// OpenAI 规范里流式中间帧的结束理由是 null，空串不是非终止信号。
pub fn rewrite_empty_finish_reason(chunk: &mut Value) -> bool {
    let Some(choices) = chunk.get_mut("choices").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    for choice in choices {
        let Some(choice) = choice.as_object_mut() else {
            continue;
        };
        if choice.get("finish_reason").and_then(Value::as_str) == Some("") {
            choice.insert("finish_reason".to_string(), Value::Null);
            changed = true;
        }
    }
    changed
}

/// 非流式聚合下发前过滤（对照 shim `strip_phantom_nonstream`）：丢掉所有
/// 无名 `tool_calls` 条目，返回丢过多少个。聚合完没有“将来分片”了，
/// 所以不用跟踪 index 命名状态 —— 无名即丢。
///
/// 调用时机是 `aggregate::into_completion` 拼 `tool_calls` 数组之前，故输入是
/// `按 index 聚好的 map`而不是最终数组。
pub fn filter_completed_tool_calls(tool_calls: &mut std::collections::BTreeMap<i64, Value>) -> u64 {
    let before = tool_calls.len();
    tool_calls.retain(|_, call| tool_call_name(call).is_some_and(|name| !name.trim().is_empty()));
    (before.saturating_sub(tool_calls.len())) as u64
}

/// 非流式终帧修正（对照 shim `strip_phantom_nonstream` 的后半段）：
/// `finish == "tool_calls"` 且聚合后没有具名调用且丢过幽灵条目
/// —— 改写为 `"stop"`。返回是否改写过。
pub fn fix_completion_finish_reason(finish: &mut String, named_kept: bool, dropped: u64) -> bool {
    if finish == "tool_calls" && !named_kept && dropped > 0 {
        finish.clear();
        finish.push_str("stop");
        return true;
    }
    false
}

fn tool_call_index(call: &Value) -> Option<i64> {
    call.get("index")
        .and_then(Value::as_i64)
        // Node：`Number.isInteger(tc.index) ? tc.index : 0`（聚合器同口径）。
        .or_else(|| call.get("index").is_none().then_some(0))
}

fn tool_call_name(call: &Value) -> Option<&str> {
    call.get("function")?.get("name")?.as_str()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn delta_of(calls: Value) -> serde_json::Map<String, Value> {
        json!({ "tool_calls": calls }).as_object().cloned().unwrap()
    }

    #[test]
    fn empty_finish_reason_rewrites_to_null() {
        let mut chunk = json!({"choices": [{"delta": {}, "finish_reason": ""}]});
        assert!(rewrite_empty_finish_reason(&mut chunk));
        assert_eq!(chunk["choices"][0]["finish_reason"], Value::Null);
    }

    #[test]
    fn non_empty_finish_reason_is_untouched() {
        for finish in ["stop", "tool_calls", "length"] {
            let mut chunk = json!({"choices": [{"delta": {}, "finish_reason": finish}]});
            assert!(!rewrite_empty_finish_reason(&mut chunk));
            assert_eq!(chunk["choices"][0]["finish_reason"], finish);
        }
        let mut null_chunk = json!({"choices": [{"delta": {}, "finish_reason": Value::Null}]});
        assert!(!rewrite_empty_finish_reason(&mut null_chunk));
    }

    #[test]
    fn phantom_stream_delta_is_dropped_and_counted() {
        let mut tracker = StreamToolCalls::new();
        let mut delta = delta_of(json!([{"index": 0, "function": {"name": "", "arguments": ""}}]));
        assert!(tracker.push(&mut delta));
        assert_eq!(tracker.dropped(), 1);
        assert!(delta.get("tool_calls").is_none());
    }

    #[test]
    fn named_stream_delta_survives_and_marks_index() {
        let mut tracker = StreamToolCalls::new();
        let mut delta = delta_of(json!([{"index": 0, "function": {"name": "pwsh", "arguments": ""}}]));
        assert!(!tracker.push(&mut delta));
        assert!(tracker.has_named());
        // 同 index 后续的空名参数分片是真调用的一部分，不丢。
        let mut args = delta_of(json!([{"index": 0, "function": {"arguments": "{\"a\":1}"}}]));
        assert!(!tracker.push(&mut args));
        assert_eq!(tracker.dropped(), 0);
    }

    #[test]
    fn phantom_finish_rewrites_to_stop_only_when_nothing_named() {
        let mut tracker = StreamToolCalls::new();
        let mut delta = delta_of(json!([{"index": 0, "function": {"name": "", "arguments": ""}}]));
        assert!(tracker.push(&mut delta));
        let mut choice = json!({"finish_reason": "tool_calls"}).as_object().cloned().unwrap();
        assert!(tracker.finish_reason(&mut choice));
        assert_eq!(choice["finish_reason"], "stop");
    }

    #[test]
    fn phantom_finish_keeps_tool_calls_when_named_exists() {
        let mut tracker = StreamToolCalls::new();
        let mut named = delta_of(json!([{"index": 0, "function": {"name": "pwsh", "arguments": "{}"}}]));
        assert!(!tracker.push(&mut named));
        // 同 index 的后续幽灵分片不会出现（已有命名记录），直接验终帧不改。
        let mut choice = json!({"finish_reason": "tool_calls"}).as_object().cloned().unwrap();
        assert!(!tracker.finish_reason(&mut choice));
        assert_eq!(choice["finish_reason"], "tool_calls");
    }

    #[test]
    fn completed_filter_drops_nameless_and_fixes_finish() {
        let mut calls = std::collections::BTreeMap::new();
        calls.insert(0, json!({"id": "", "type": "function", "function": {"name": "", "arguments": ""}}));
        let dropped = filter_completed_tool_calls(&mut calls);
        assert_eq!(dropped, 1);
        assert!(calls.is_empty());
        let mut finish = "tool_calls".to_string();
        assert!(fix_completion_finish_reason(&mut finish, false, dropped));
        assert_eq!(finish, "stop");
    }
}
