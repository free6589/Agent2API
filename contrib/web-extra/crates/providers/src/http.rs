//! 进程内小工具：Unix 毫秒时间戳。
//!
//! 本包只有 qwen-web 一家，控制面请求的客户端与响应解析都收在
//! `providers/qwen` 模块内，这里不再放公共 HTTP 层 —— 没有第二家会用到它。

/// Unix 毫秒时间戳；时钟早于 epoch 时给 0（只用于请求体时间戳与 chunk 时间）。
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}
