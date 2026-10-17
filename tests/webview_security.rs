//! Exercise the security modules without starting the launcher or a display server.
#![allow(dead_code)]

rust_i18n::i18n!("locales", fallback = "en");

#[path = "../src/commands.rs"]
mod commands;
#[path = "../src/popup_guard.rs"]
mod popup_guard;
#[path = "../src/preferences.rs"]
mod preferences;
#[path = "../src/ui/mod.rs"]
mod ui;
#[path = "../src/webview_security.rs"]
mod webview_security;
