//! Desktop shell for the hub's authenticated local board.
//!
//! The board itself is served by the hub on loopback. This window only finds its address and the
//! current token in the hub's private state (the same files `communication-hub board --open` uses),
//! follows token rotation after hub restarts, and refuses to navigate anywhere else.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
use serde_json::Value;
use std::{path::PathBuf, time::Duration};
use tauri::{Url, WebviewUrl, WebviewWindowBuilder};

/// The CLI's default configuration names the hub's state directory.
fn state_dir() -> Option<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME")?);
    let config = home.join("Library/Application Support/CommunicationHub/config.json");
    let config: Value = serde_json::from_slice(&std::fs::read(config).ok()?).ok()?;
    Some(PathBuf::from(config["state"].as_str()?))
}
fn read(path: PathBuf) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}
/// The authenticated board URL, or `None` while the hub's board is offline.
fn board_url() -> Option<Url> {
    let state = state_dir()?;
    let status = read(state.join("dashboard-status.json"))?;
    if status["online"] != true {
        return None;
    }
    let origin = status["url"].as_str()?;
    if !origin.starts_with("http://127.0.0.1:") {
        return None;
    }
    let token = read(state.join("dashboard-auth.json"))?["token"]
        .as_str()?
        .to_owned();
    if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // The token travels in the fragment, which the board script moves to session storage.
    Url::parse(&format!("{origin}/#token={token}")).ok()
}
fn is_board(url: &Url) -> bool {
    url.scheme() == "http" && url.host_str() == Some("127.0.0.1")
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let first = board_url();
            let start = match &first {
                Some(url) => WebviewUrl::External(url.clone()),
                None => WebviewUrl::App("index.html".into()),
            };
            let window = WebviewWindowBuilder::new(app, "board", start)
                .title("Communication Hub")
                .inner_size(1320.0, 900.0)
                .min_inner_size(900.0, 600.0)
                // Only the local board (and the bundled waiting page) may load in this window.
                .on_navigation(|url| is_board(url) || url.scheme() == "tauri")
                .build()?;
            // Follow hub restarts: a new board token means a new authenticated URL.
            std::thread::spawn(move || {
                let mut current = first;
                loop {
                    std::thread::sleep(Duration::from_secs(3));
                    let next = board_url();
                    if next.is_some() && next != current {
                        if let Some(url) = next.clone() {
                            let _ = window.navigate(url);
                        }
                        current = next;
                    }
                }
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("failed to run the Communication Hub board");
}
