//! claude-history-pick: Claude Code / Codex CLI のプロンプト履歴を fzf で選択して Zed に貼り付ける。
//!
//! ## 処理フロー
//!
//! 1. guard     — 先行インスタンスを排除して単一インスタンス権を取得
//! 2. claude / codex — 各ツールの history.jsonl を読み込み、history::merge_sort_dedup で統合
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

mod claude;
mod clipboard;
mod codex;
mod guard;
mod history;
mod injector;
mod picker;

use history::Prompt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// タスクターミナルが閉じ始めるのを待つ最小時間。
///
/// この後 injector の AppleScript が「Zed 前面化」を確認するまで最大 40×50ms = 2 秒
/// ポーリングする。この定数はポーリング開始までの最低保証時間であり、
/// ポーリング継続時間とは直交する。値を変えるなら injector の polling 定数と合わせて検討する。
const PASTE_DELAY: Duration = Duration::from_millis(100);

/// 履歴ファイルパスを環境変数から解決する際の探索候補。
///
/// - `Direct(var)`: `var` の値をそのままファイルパスとして使う
///   （例: `CLAUDE_HISTORY_PATH`）。
/// - `DirJoinFile(var, file_name)`: `var` はディレクトリを指す値とみなし
///   `var/file_name` を使う（例: `CODEX_HOME` は Codex CLI 自身が
///   history.jsonl の保存先ディレクトリとして尊重する環境変数）。
enum EnvPathSource {
    Direct(&'static str),
    DirJoinFile(&'static str, &'static str),
}

/// `sources` を先頭から順に調べ、最初に設定されている環境変数からパスを解決する。
/// どれも未設定なら `$HOME/{default_relative}` にフォールバックする。
///
/// 戻り値の `bool` は環境変数から取得した場合 `true`（エラーメッセージの出し分け・
/// `load_source` の fatal/skip 判定に使う）。ソース別の `resolve_*_history_path` は
/// この関数に候補リストを渡すだけの薄いラッパーにすることで、探索ロジック自体の
/// 重複（≒ 将来ソースを追加するたびに同じ if-let チェーンを書き写すリスク）を避ける。
fn resolve_history_path(sources: &[EnvPathSource], default_relative: &str) -> (PathBuf, bool) {
    for source in sources {
        match source {
            EnvPathSource::Direct(var) => {
                if let Ok(p) = std::env::var(var) {
                    return (PathBuf::from(p), true);
                }
            }
            EnvPathSource::DirJoinFile(var, file_name) => {
                if let Ok(dir) = std::env::var(var) {
                    return (PathBuf::from(dir).join(file_name), true);
                }
            }
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    (PathBuf::from(home).join(default_relative), false)
}

/// Claude Code の履歴ファイルのパスを解決する。
/// `CLAUDE_HISTORY_PATH` 環境変数が設定されていればそのパスを使う。
/// 未設定なら `$HOME/.claude/history.jsonl` を返す。
fn resolve_claude_history_path() -> (PathBuf, bool) {
    resolve_history_path(
        &[EnvPathSource::Direct("CLAUDE_HISTORY_PATH")],
        ".claude/history.jsonl",
    )
}

/// Codex CLI の履歴ファイルのパスを解決する。
///
/// 優先順位: `CODEX_HISTORY_PATH`（本ツール専用の明示 override）→
/// `CODEX_HOME`（Codex CLI 自身が history.jsonl の保存先として尊重する環境変数。
/// 既に設定済みの user は追加設定なしで動く）→ `$HOME/.codex/history.jsonl`。
fn resolve_codex_history_path() -> (PathBuf, bool) {
    resolve_history_path(
        &[
            EnvPathSource::Direct("CODEX_HISTORY_PATH"),
            EnvPathSource::DirJoinFile("CODEX_HOME", "history.jsonl"),
        ],
        ".codex/history.jsonl",
    )
}

/// 1 ソース分の履歴を読み込む。
///
/// - デフォルトパスが存在しない（`NotFound`）: そのツールを使っていない環境として
///   warning を出し空 `Vec` を返す（他ソースのみで動作継続）。
/// - env var で明示指定したパスのエラー、またはそれ以外の IO エラー（Permission
///   denied・EIO 等）: 履歴欠落をサイレントに招くため fatal としてプロセスを終了する。
fn load_source<F: FnOnce(&Path) -> io::Result<Vec<Prompt>>>(
    name: &str,
    path: &Path,
    from_env_var: bool,
    loader: F,
) -> Vec<Prompt> {
    match loader(path) {
        Ok(v) => v,
        Err(e) if e.kind() == io::ErrorKind::NotFound && !from_env_var => {
            eprintln!(
                "[{name}] 履歴ファイルが見つからないためスキップします（{}）",
                path.display()
            );
            Vec::new()
        }
        Err(e) => {
            if from_env_var {
                eprintln!(
                    "[{name}] 環境変数で指定したパスが見つかりません: {}",
                    path.display()
                );
            } else {
                eprintln!("[{name}] 履歴の読み込みに失敗: {}", path.display());
            }
            eprintln!("エラー: {e}");
            guard::release();
            std::process::exit(1);
        }
    }
}

fn main() {
    guard::acquire();

    let (claude_path, claude_from_env) = resolve_claude_history_path();
    let (codex_path, codex_from_env) = resolve_codex_history_path();

    let mut all_prompts = Vec::new();
    all_prompts.extend(load_source(
        "Claude",
        &claude_path,
        claude_from_env,
        claude::load_claude_prompts,
    ));
    all_prompts.extend(load_source(
        "Codex",
        &codex_path,
        codex_from_env,
        codex::load_codex_prompts,
    ));

    let prompts = history::merge_sort_dedup(all_prompts);

    if prompts.is_empty() {
        eprintln!("履歴が見つかりませんでした");
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
    // CLAUDE_HISTORY_PATH / CODEX_HISTORY_PATH / CODEX_HOME すべてを同じ Mutex で保護する。
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn clear_all_history_env_vars() {
        std::env::remove_var("CLAUDE_HISTORY_PATH");
        std::env::remove_var("CODEX_HISTORY_PATH");
        std::env::remove_var("CODEX_HOME");
    }

    #[test]
    fn claude_env_var_set_returns_custom_path_and_true() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_all_history_env_vars();
        std::env::set_var("CLAUDE_HISTORY_PATH", "/custom/path/history.jsonl");
        let (path, from_env_var) = resolve_claude_history_path();
        clear_all_history_env_vars();
        assert_eq!(path, PathBuf::from("/custom/path/history.jsonl"));
        assert!(from_env_var);
    }

    #[test]
    fn claude_env_var_unset_returns_default_path_and_false() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_all_history_env_vars();
        let (path, from_env_var) = resolve_claude_history_path();
        assert!(path.to_string_lossy().ends_with(".claude/history.jsonl"));
        assert!(!from_env_var);
    }

    #[test]
    fn claude_default_path_is_under_home() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_all_history_env_vars();
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let (path, _) = resolve_claude_history_path();
        assert_eq!(path, PathBuf::from(&home).join(".claude/history.jsonl"));
    }

    #[test]
    fn claude_home_unset_falls_back_to_dot() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_all_history_env_vars();
        let original_home = std::env::var("HOME").ok();
        std::env::remove_var("HOME");
        let (path, from_env_var) = resolve_claude_history_path();
        match original_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        assert_eq!(path, PathBuf::from("./.claude/history.jsonl"));
        assert!(!from_env_var);
    }

    #[test]
    fn claude_env_var_empty_string_is_accepted_as_env_var_path() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_all_history_env_vars();
        std::env::set_var("CLAUDE_HISTORY_PATH", "");
        let (path, from_env_var) = resolve_claude_history_path();
        clear_all_history_env_vars();
        assert_eq!(path, PathBuf::from(""));
        assert!(from_env_var);
    }

    // ─── クロスセッション可視性の根拠: CWD に依存しない HOME-relative パス ───

    #[test]
    fn claude_history_path_is_home_relative_not_cwd_relative() {
        // 「どの terminal から起動しても同じ history.jsonl を読む」保証の根拠。
        // resolve_claude_history_path() は CWD に依存せず HOME/.claude/history.jsonl を返す。
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_all_history_env_vars();
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let expected = PathBuf::from(&home).join(".claude/history.jsonl");

        let original_cwd = std::env::current_dir().ok();
        let _ = std::env::set_current_dir("/tmp");
        let (path_from_tmp, _) = resolve_claude_history_path();
        if let Some(cwd) = original_cwd {
            let _ = std::env::set_current_dir(cwd);
        }

        assert_eq!(
            path_from_tmp, expected,
            "CWD が /tmp でも ~/.claude/history.jsonl を指すべき"
        );
    }

    // ─── Codex パス解決 ─────────────────────────────────────────────────────

    #[test]
    fn codex_history_path_env_var_takes_priority() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_all_history_env_vars();
        std::env::set_var("CODEX_HISTORY_PATH", "/custom/codex-history.jsonl");
        std::env::set_var("CODEX_HOME", "/should/be/ignored");
        let (path, from_env_var) = resolve_codex_history_path();
        clear_all_history_env_vars();
        assert_eq!(path, PathBuf::from("/custom/codex-history.jsonl"));
        assert!(from_env_var);
    }

    #[test]
    fn codex_home_is_used_when_history_path_unset() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_all_history_env_vars();
        std::env::set_var("CODEX_HOME", "/fake/codex-home");
        let (path, from_env_var) = resolve_codex_history_path();
        clear_all_history_env_vars();
        assert_eq!(path, PathBuf::from("/fake/codex-home/history.jsonl"));
        assert!(from_env_var, "CODEX_HOME 経由も明示指定として扱うべき");
    }

    #[test]
    fn codex_default_path_is_under_home_when_no_env_vars() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_all_history_env_vars();
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let (path, from_env_var) = resolve_codex_history_path();
        assert_eq!(path, PathBuf::from(&home).join(".codex/history.jsonl"));
        assert!(!from_env_var);
    }

    // ─── load_source ────────────────────────────────────────────────────────

    #[test]
    fn load_source_returns_loader_result_on_success() {
        let result = load_source("Test", Path::new("/unused"), false, |_| {
            Ok(vec![history::test_support::make_prompt(
                history::Source::Claude,
                "ok",
                Some(100),
            )])
        });
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn load_source_default_path_not_found_returns_empty_without_exit() {
        // デフォルトパス（from_env_var = false）で NotFound は非致命: 空 Vec を返し継続する。
        let result = load_source(
            "Test",
            Path::new("/definitely/does/not/exist/history.jsonl"),
            false,
            claude::load_claude_prompts,
        );
        assert!(result.is_empty());
    }
}
