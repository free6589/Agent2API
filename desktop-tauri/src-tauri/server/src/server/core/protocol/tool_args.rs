//! 参数引号数字归一化（`server-shim` v2.7 的干净版）。
//!
//! 为什么需要它 ───────────────────────────────
//! flash 级别模型会把可选数值参数发成字符串（`{"limit":"10"}`），
//! grok 在 PreToolUse hook 之前用严格 serde 解析 arguments，一个
//! `invalid type: string "10", expected u8` 就烧掉整个调用。
//! 请求体自带每个工具的 JSON schema，数值字段集从那里读
//! 而不是硬编码：修复对路过的任何客户端/模型都精确，
//! 字符串字段（电话号码、日期、页码区间）永远碰不到。
//!
//! 三条硬约束（与 shim 同口径）────────────────
//! 1. 只认本次请求 `tools[].function.parameters.properties` 声明为
//!    `integer` / `number`（含 `["integer","null"]` 联合）的字段，附
//!    `minimum` / `maximum` 边界校验；
//! 2. 越界 / 前导零 / 空串保持原样，不 clamp；
//! 3. 只有完整、可解析的参数载荷才重写，其余字节级透传。
//!
//! release 是 `panic=abort`：本模块零 unwrap/expect，取值一律走 Option 链。

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// `"10"` → 10 用的整形判定（shim 的 `INT_ARG_RE`，同口径手写）。
///
/// 前导零保留原样：`010` 哲学上可能是八进制，而不是十进制 10，
/// 减法前导零会让不同语言的客户端解出不同的数。
///
/// 不用 regex：这两个创格简单到手写更可靠，还省一次编译期依赖。
fn is_int_shape(text: &str) -> bool {
    let digits = text
        .strip_prefix('+')
        .or_else(|| text.strip_prefix('-'))
        .unwrap_or(text);
    if digits == "0" {
        return true;
    }
    let mut chars = digits.chars();
    match chars.next() {
        Some('1'..='9') => chars.all(|c| c.is_ascii_digit()),
        _ => false,
    }
}

/// `"10.5"` → 10.5 用的字面量判定（shim 的 `NUM_ARG_RE`，同口径手写）。
///
/// 返回 `Some(带不带小数点)`，形状不对返回 None。
fn num_shape(text: &str) -> Option<bool> {
    let digits = text
        .strip_prefix('+')
        .or_else(|| text.strip_prefix('-'))
        .unwrap_or(text);
    let mut parts = digits.split('.');
    let int_part = parts.next()?;
    if !is_int_shape(int_part) {
        return None;
    }
    match parts.next() {
        // 无小数点：纯整数形状
        None => Some(false),
        Some(frac) => {
            // 第二个点有多余分片；空小数点（`10.`）也不算数
            if parts.next().is_some() {
                return None;
            }
            if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            Some(true)
        }
    }
}

/// 工具名 → 字段名 → (类型, 下界, 上界)。
///
/// 映射表只含数值声明的字段：只有告诉网关「可以修」
/// 的，网关才修。
#[derive(Default, Debug, Clone)]
pub struct NumericFields {
    inner: BTreeMap<String, BTreeMap<String, (NumKind, Option<f64>, Option<f64>)>>,
}

/// 一个声明的数值类型（`pub(crate)`：调用方只关心「修 / 不修」，
/// 具体 kind 是内部判定细节，不对外暴露）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NumKind {
    Integer,
    Number,
}

impl NumericFields {
    /// 本次请求的 tools 声明 → 数值字段映射表。
    ///
    /// 只认 `tools[].function.parameters.properties` 声明为 integer /
    /// number（含 nullable union）的字段；其余字段（电话号码、
    /// 日期……）不进表，决不碰。
    pub fn build_numeric_map(request: &Value) -> Self {
        let mut out = NumericFields::default();
        let Some(tools) = request.get("tools").and_then(Value::as_array) else {
            return out;
        };
        for tool in tools {
            let Some(function) = tool.get("function") else {
                continue;
            };
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let Some(properties) = function
                .get("parameters")
                .and_then(|parameters| parameters.get("properties"))
                .and_then(Value::as_object)
            else {
                continue;
            };
            if name.is_empty() {
                continue;
            }
            let mut fields = BTreeMap::new();
            for (field, declared) in properties {
                let Some(kind) = num_kind_of(declared.get("type")) else {
                    continue;
                };
                fields.insert(
                    field.clone(),
                    (
                        kind,
                        schema_bound(declared.get("minimum")),
                        schema_bound(declared.get("maximum")),
                    ),
                );
            }
            if !fields.is_empty() {
                out.inner.insert(name, fields);
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// 某工具的某字段是否声明为数值（`pub(crate)`：同模块族内部用）。
    pub(crate) fn field_of(&self, tool: &str, field: &str) -> Option<(NumKind, Option<f64>, Option<f64>)> {
        self.inner.get(tool)?.get(field).copied()
    }
}

/// JSON-schema `type`（字符串或 nullable union）→ 数值类型。
///
/// `integer` 优先于 `number`：union 里两者都有时，整形
/// 创格更严、更安全（修复后的值一定能过整形校验）。
fn num_kind_of(declared: Option<&Value>) -> Option<NumKind> {
    let mut kind: Option<NumKind> = None;
    match declared {
        Some(Value::String(single)) if single == "integer" => Some(NumKind::Integer),
        Some(Value::String(single)) if single == "number" => Some(NumKind::Number),
        Some(Value::Array(union)) => {
            for member in union {
                match member.as_str() {
                    Some("integer") => {
                        kind = Some(NumKind::Integer);
                        break;
                    }
                    Some("number") => kind = Some(NumKind::Number),
                    _ => {}
                }
            }
            kind
        }
        _ => None,
    }
}

/// schema 的 minimum/maximum：只有有限数值才算数，bool 不算。
fn schema_bound(value: Option<&Value>) -> Option<f64> {
    value?.as_f64().filter(|number| number.is_finite())
}

/// 引号数字 → 整形/浮点，拿不准返回 None（保持原样）。
///
/// 返回 None 的几种情况：不是数字形状（前导零、空串、
/// 电话号码含 - 等）、解析失败、越界（schema 有
/// minimum/maximum 时）。越界保留原样而不是 clamp：保留了
/// 原始错误信息可观测，上游还是会 400，模型自己会改。
pub fn coerce(text: &str, kind: NumKind, lo: Option<f64>, hi: Option<f64>) -> Option<Value> {
    let trimmed = text.trim();
    let number: f64 = match kind {
        NumKind::Integer => {
            if !is_int_shape(trimmed) {
                return None;
            }
            trimmed.parse::<i64>().ok()? as f64
        }
        NumKind::Number => {
            let has_dot = match num_shape(trimmed) {
                Some(has_dot) => has_dot,
                None => return None,
            };
            if has_dot {
                trimmed.parse::<f64>().ok()?
            } else {
                trimmed.parse::<i64>().ok()? as f64
            }
        }
    };
    if !number.is_finite() {
        return None;
    }
    if lo.is_some_and(|lo| number < lo) || hi.is_some_and(|hi| number > hi) {
        return None;
    }
    if kind == NumKind::Integer {
        // i64::MIN..=i64::MAX 是 parse 保证的，转换不会越界。
        Some(Value::from(number as i64))
    } else if trimmed.contains('.') {
        Some(Value::from(number))
    } else {
        Some(Value::from(number as i64))
    }
}

/// 已解析的 arguments 对象就地修复：返回是否改动。
///
/// 只动明确为字符串的值：数字本体、bool、null、数组、
/// 对象一律不动。
pub fn fix_object(
    args: &mut Map<String, Value>,
    fields: &BTreeMap<String, (NumKind, Option<f64>, Option<f64>)>,
) -> bool {
    let mut changed = false;
    for (field, (kind, lo, hi)) in fields {
        let Some(raw) = args.get(field).and_then(Value::as_str) else {
            continue;
        };
        if let Some(number) = coerce(raw, *kind, *lo, *hi) {
            args.insert(field.clone(), number);
            changed = true;
        }
    }
    changed
}

/// 工具名 + arguments 字符串 → 修复后的 arguments 字符串。
///
/// 不可解析 / 无匹配字段 / 无改动时返回 None（调用方保持原样）。
pub fn fix_args_str(map: &NumericFields, tool: &str, raw: &str) -> Option<String> {
    let fields = map.inner.get(tool)?;
    let mut args: Value = serde_json::from_str(raw).ok()?;
    let changed = fix_object(args.as_object_mut()?, fields);
    if !changed {
        return None;
    }
    serde_json::to_string(&args).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request_with_props(props: Value) -> Value {
        json!({
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_items",
                    "parameters": { "type": "object", "properties": props }
                }
            }]
        })
    }

    /// `{"limit":"10"}` → `{"limit":10}`：最基本的修复。
    #[test]
    fn quoted_integer_is_coerced() {
        let map = NumericFields::build_numeric_map(
            &request_with_props(json!({ "limit": { "type": "integer" } })),
        );
        assert_eq!(
            fix_args_str(&map, "get_items", r#"{"limit":"10"}"#),
            Some(r#"{"limit":10}"#.to_string())
        );
    }

    /// `"010"` 不动：哲学上可能是八进制，减法前导零会歪曲语义。
    #[test]
    fn leading_zero_is_kept() {
        let map = NumericFields::build_numeric_map(
            &request_with_props(json!({ "limit": { "type": "integer" } })),
        );
        assert_eq!(fix_args_str(&map, "get_items", r#"{"limit":"010"}"#), None);
    }

    /// 越界保持原样：让上游 400 报错可观测，模型自己会改，
    /// 网关不 clamp。
    #[test]
    fn out_of_range_is_kept() {
        let map = NumericFields::build_numeric_map(&request_with_props(
            json!({ "limit": { "type": "integer", "minimum": 1, "maximum": 5 } }),
        ));
        assert_eq!(fix_args_str(&map, "get_items", r#"{"limit":"10"}"#), None);
    }

    /// 电话号码不动：没声明为数值的字段决不碰。
    #[test]
    fn phone_string_is_kept() {
        let map = NumericFields::build_numeric_map(
            &request_with_props(json!({ "phone": { "type": "string" } })),
        );
        assert_eq!(
            fix_args_str(&map, "get_items", r#"{"phone":"13800138000"}"#),
            None
        );
    }

    /// nullable union `["integer","null"]` 生效（schemars 对 Option<u8>
    /// 的渲染，flash 级别模型最容易发成字符串的就是这种）。
    #[test]
    fn nullable_union_is_coerced() {
        let map = NumericFields::build_numeric_map(&request_with_props(
            json!({ "limit": { "type": ["integer", "null"] } }),
        ));
        assert_eq!(
            fix_args_str(&map, "get_items", r#"{"limit":"10"}"#),
            Some(r#"{"limit":10}"#.to_string())
        );
    }

    /// `"10.5"` → 10.5；整形字段拒绝小数。
    #[test]
    fn number_and_integer_kinds() {
        let map = NumericFields::build_numeric_map(&request_with_props(
            json!({ "ratio": { "type": "number" }, "limit": { "type": "integer" } }),
        ));
        assert_eq!(
            fix_args_str(&map, "get_items", r#"{"ratio":"10.5"}"#),
            Some(r#"{"ratio":10.5}"#.to_string())
        );
        assert_eq!(
            fix_args_str(&map, "get_items", r#"{"limit":"10.5"}"#),
            None
        );
    }

    /// 空串保持原样。
    #[test]
    fn empty_string_is_kept() {
        let map = NumericFields::build_numeric_map(
            &request_with_props(json!({ "limit": { "type": "integer" } })),
        );
        assert_eq!(fix_args_str(&map, "get_items", r#"{"limit":""}"#), None);
    }
}
