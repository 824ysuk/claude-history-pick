//! claude-history-pick: Claude Code プロンプト履歴を fzf で選択して Zed に貼り付ける。
//!
//! ## 処理フロー
//!
//! 1. history    — ~/.claude/history.jsonl を読み込み、表示用候補を作成
//! 2. picker     — fzf を起動してユーザーに選択させる
//! 3. clipboard  — 選択テキストを pbcopy でクリップボードにセット
//! 4. injector   — double-fork でデーモン化し、delay 後に cmd-r を Zed へ送信
//!                  → Zed の terminal::Paste がクリップボードを Claude Code に貼り付ける
//!
//! ## 依存
//!
//! - fzf: brew install fzf
//! - pbcopy: macOS 標準（追加インストール不要）
//! - osascript: macOS 標準（追加インストール不要）
//! - serde_json, libc: Cargo.toml 参照

mod clipboard;
mod history;
mod picker;
mod injector;

use std::path::PathBuf;
use std::time::Duration;

/// fzf 選択からクリップボードセットまでの間に待つ時間。
/// Zed が hide: on_success でターミナルタブを閉じてフォーカスが
/// 前ターミナルに戻るまでの時間を確保する。
const PASTE_DELAY: Duration = Duration::from_millis(300);

fn main() {
    // ~/.claude/history.jsonl のパスを構築
    let history_path = {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let mut p = PathBuf::from(home);
        p.push(".claude/history.jsonl");
        p
    };

    // 履歴の読み込み・パース・フィルタリング
    let prompts = match history::load_prompts(&history_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("history.jsonl の読み込みに失敗: {e}");
            eprintln!("パス: {}", history_path.display());
            std::process::exit(1);
        }
    };

    if prompts.is_empty() {
        eprintln!("履歴が見つかりませんでした（{}）", history_path.display());
        std::process::exit(0);
    }

    // fzf で候補を表示し、ユーザーの選択を得る
    let selected = match picker::pick(&prompts) {
        Some(s) => s,
        None => std::process::exit(0), // Esc / キャンセル
    };

    // 選択テキストをクリップボードにセット（terminal::Paste の貼り付け元）
    if let Err(e) = clipboard::copy_to_clipboard(&selected) {
        eprintln!("クリップボードへのコピーに失敗: {e}");
        std::process::exit(1);
    }

    // double-fork で Zed のプロセスグループから切り離し、
    // タスクターミナルが閉じてフォーカスが戻った後に cmd-r を発火
    injector::inject_keystroke_after_delay(PASTE_DELAY);
}
