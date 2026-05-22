#![allow(
    clippy::cargo_common_metadata,
    clippy::format_push_string,
    clippy::missing_const_for_fn,
    clippy::missing_errors_doc,
    clippy::multiple_crate_versions,
    clippy::must_use_candidate,
    clippy::needless_raw_string_hashes,
    clippy::option_if_let_else,
    clippy::semicolon_if_nothing_returned,
    clippy::similar_names,
    clippy::struct_excessive_bools,
    clippy::uninlined_format_args,
    clippy::unnecessary_join,
    clippy::unused_async
)]

pub mod account;
pub mod admin;
pub mod auth;
pub mod backup;
pub mod cli;
pub mod compression;
pub mod config;
pub mod csrf;
pub mod db;
pub mod demo_seed;
pub mod errors;
pub mod favicon;
pub mod ffmpeg;
pub mod logging;
pub mod media;
pub mod rate_limit;
pub mod render;
pub mod runtime;
pub mod server;
pub mod social;
pub mod terminal;
pub mod tor;
pub mod validation;
