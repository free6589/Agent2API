//! Agent2API 网页提供商扩展包入口：只承载通义千问网页（qwen-web）一家。
//!
//! 包形态与官方网页包（`agent2api-web-providers`）一致：`pack.json` 声明
//! `protocol: 1` 与五平台二进制入口，进程体只做一件事 —— 把 registry 交给
//! SDK 的 `serve_blocking`，之后的分帧、回调与背压都由 SDK 处理。
//!
//! 与官方包的差异：本包是第三方包（不在 `OFFICIAL_PACKS` 名单里），走
//! 「从文件安装」，没有自动更新与签名通道；见 `README.md` 的安装说明。

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod http;
mod providers;

fn main() {
    agent2api_provider_sdk::serve_blocking(providers::registry());
}
