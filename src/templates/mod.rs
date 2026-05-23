//! Server-rendered HTML. The page is a thin shell — it loads the embedded
//! `xterm.js` module and `main.js`, which then opens the WebSocket channels.
//!
//! The shell intentionally renders without inline scripts so the CSP can stay
//! `script-src 'self'`.

use maud::{DOCTYPE, Markup, PreEscaped, html};

pub fn index_page() -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover";
                meta name="color-scheme" content="dark light";
                title { "tmons" }
                link rel="stylesheet" href="/assets/vendor/xterm.css";
                link rel="stylesheet" href="/assets/main.css";
                // No favicon: avoid the 404 noise without inviting CSP gymnastics.
                link rel="icon" href="data:,";
            }
            body {
                div #app {
                    aside #sidebar {
                        header {
                            h1 { "tmons" }
                            button #sidebar-toggle aria-label="Toggle sidebar" { "☰" }
                        }
                        ul #session-list aria-live="polite" aria-label="tmux sessions" {
                            li.empty-hint {
                                "Loading sessions…"
                            }
                        }
                    }
                    main #pane-area aria-label="focused tmux pane" {
                        div #terminal-container { }
                        div #key-row.hidden role="toolbar" aria-label="terminal keys" {
                            button data-key="esc" { "Esc" }
                            button data-key="tab" { "Tab" }
                            button data-key="ctrl-c" { "Ctrl-C" }
                            button data-key="up" { "↑" }
                            button data-key="down" { "↓" }
                            button data-key="left" { "←" }
                            button data-key="right" { "→" }
                        }
                    }
                }
                (PreEscaped(r#"<script type="module" src="/assets/main.js"></script>"#))
            }
        }
    }
}
