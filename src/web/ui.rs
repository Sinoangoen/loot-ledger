//! The dashboard, compiled into the binary.
//!
//! Three static files and nothing else: no build step, no npm, no bundler. The
//! user runs one binary and opens a URL.

/// The page shell.
pub const INDEX_HTML: &str = include_str!("assets/index.html");

/// Dashboard behaviour.
pub const APP_JS: &str = include_str!("assets/app.js");

/// Styling.
pub const STYLE_CSS: &str = include_str!("assets/style.css");
