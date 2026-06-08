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

/// 履歴ファイルのパスを解決する。
///
/// `CLAUDE_HISTORY_PATH` 環境変数が設定されていればそのパスを使う。
/// 未設定なら `$HOME/.claude/history.jsonl` を返す。
/// 戻り値の `bool` は環境変数から取得した場合 `true`（エラーメッセージの出し分けに使う）。
fn resolve_history_path() -> (PathBuf, bool) {
    if let Ok(p) = std::env::var("CLAUDE_HISTORY_PATH") {
        (PathBuf::from(p), true)
    } else {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        (PathBuf::from(home).join(".claude/history.jsonl"), false)
    }
}

fn main() {
    guard::acquire();

    let (history_path, from_env_var) = resolve_history_path();

    let prompts = match history::load_prompts(&history_path) {
        Ok(p) => p,
        Err(e) => {
            if from_env_var {
                eprintln!(
                    "CLAUDE_HISTORY_PATH で指定したパスが見つかりません: {}",
                    history_path.display()
                );
                eprintln!("エラー: {e}");
            } else {
                eprintln!("history.jsonl の読み込みに失敗: {e}");
                eprintln!("パス: {}", history_path.display());
            }
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
    if let Err(e) = injector::inject_keystroke_after_delay(PASTE_DELAY) {
        // setsid() / spawn() 失敗時。クリップボードへのコピーは成功しているため、
        // 「自動貼り付けが効かなかったが内容はコピー済み」であることを伝達する。
        eprintln!("osascript の起動に失敗しました: {e}");
        eprintln!(
            "選択したプロンプトはクリップボードにコピー済みです。Zed の入力欄に手動で cmd-r または cmd-v で貼り付けてください。"
        );
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 環境変数はプロセス全体に影響するため、並列テストが互いを汚染しないよう直列化する。
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn env_var_set_returns_custom_path_and_true() {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("CLAUDE_HISTORY_PATH", "/custom/path/history.jsonl");
        let (path, from_env_var) = resolve_history_path();
        std::env::remove_var("CLAUDE_HISTORY_PATH");
        assert_eq!(path, PathBuf::from("/custom/path/history.jsonl"));
        assert!(from_env_var);
    }

    #[test]
    fn env_var_unset_returns_default_path_and_false() {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::remove_var("CLAUDE_HISTORY_PATH");
        let (path, from_env_var) = resolve_history_path();
        assert!(path.to_string_lossy().ends_with(".claude/history.jsonl"));
        assert!(!from_env_var);
    }

    #[test]
    fn default_path_is_under_home() {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::remove_var("CLAUDE_HISTORY_PATH");
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let (path, _) = resolve_history_path();
        assert_eq!(path, PathBuf::from(&home).join(".claude/history.jsonl"));
    }

    #[test]
    fn home_unset_falls_back_to_dot() {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::remove_var("CLAUDE_HISTORY_PATH");
        let original_home = std::env::var("HOME").ok();
        std::env::remove_var("HOME");
        let (path, from_env_var) = resolve_history_path();
        // HOME を元に戻す
        match original_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        assert_eq!(path, PathBuf::from("./.claude/history.jsonl"));
        assert!(!from_env_var);
    }

    #[test]
    fn env_var_empty_string_is_accepted_as_env_var_path() {
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::set_var("CLAUDE_HISTORY_PATH", "");
        let (path, from_env_var) = resolve_history_path();
        std::env::remove_var("CLAUDE_HISTORY_PATH");
        assert_eq!(path, PathBuf::from(""));
        assert!(from_env_var);
    }

    // ─── クロスセッション可視性の根拠: CWD に依存しない HOME-relative パス ───

    #[test]
    fn history_path_is_home_relative_not_cwd_relative() {
        // 「どの terminal から起動しても同じ history.jsonl を読む」保証の根拠。
        // resolve_history_path() は CWD に依存せず HOME/.claude/history.jsonl を返す。
        // dotfiles worktree / 別 repo / 直接起動 — いずれも同一ファイルを指す。
        let _guard = ENV_MUTEX.lock().unwrap();
        std::env::remove_var("CLAUDE_HISTORY_PATH");
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let expected = PathBuf::from(&home).join(".claude/history.jsonl");

        // CWD を一時的に /tmp に変えて呼び出す
        let original_cwd = std::env::current_dir().ok();
        let _ = std::env::set_current_dir("/tmp");
        let (path_from_tmp, _) = resolve_history_path();
        if let Some(cwd) = original_cwd {
            let _ = std::env::set_current_dir(cwd);
        }

        assert_eq!(
            path_from_tmp, expected,
            "CWD が /tmp でも ~/.claude/history.jsonl を指すべき"
        );
    }
}
