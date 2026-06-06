//! claude-history-pick: Claude Code プロンプト履歴を fzf で選択して Zed に貼り付ける。
//!
//! ## 処理フロー
//!
//! 1. guard     — 先行インスタンスを排除して単一インスタンス権を取得
//! 2. history   — ~/.claude/history.jsonl を読み込み、表示用候補を作成
//! 3. picker    — fzf を起動してユーザーに選択させる
//! 4. clipboard — 選択テキストを pbcopy でクリップボードにセット
//! 5. guard     — ロックを解放（後続の ctrl-; r を妨げない）
//! 6. injector  — setsid で独立した osascript を起動し、Zed へ cmd-r を送信
//!
//! ## 依存
//!
//! - fzf: brew install fzf
//! - pbcopy: macOS 標準（追加インストール不要）
//! - osascript: macOS 標準（追加インストール不要）
//! - serde_json, libc: Cargo.toml 参照

mod clipboard;
mod guard;
mod history;
mod injector;
mod picker;

use std::path::PathBuf;
use std::time::Duration;

/// タスクターミナルが閉じ始めるのを待つ最小時間。
/// この後 AppleScript のポーリングで Zed が前面に来るまで待機するため、
/// 固定値への依存は排除されている。
const PASTE_DELAY: Duration = Duration::from_millis(100);

fn main() {
    guard::acquire();

    let history_path = {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let mut p = PathBuf::from(home);
        p.push(".claude/history.jsonl");
        p
    };

    let prompts = match history::load_prompts(&history_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("history.jsonl の読み込みに失敗: {e}");
            eprintln!("パス: {}", history_path.display());
            guard::release();
            std::process::exit(1);
        }
    };

    if prompts.is_empty() {
        eprintln!("履歴が見つかりませんでした（{}）", history_path.display());
        guard::release();
        std::process::exit(0);
    }

    let selected = match picker::pick(&prompts) {
        Some(s) => s,
        None => {
            guard::release();
            std::process::exit(0); // Esc / キャンセル
        }
    };

    if let Err(e) = clipboard::copy_to_clipboard(&selected) {
        eprintln!("クリップボードへのコピーに失敗: {e}");
        guard::release();
        std::process::exit(1);
    }

    // injector が fork するため、ロックは fork 前に解放する。
    // daemon は main process 終了後も動くが、次の ctrl-; r を妨げない。
    guard::release();
    injector::inject_keystroke_after_delay(PASTE_DELAY);
}
