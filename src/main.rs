//! agent-history-pick: Claude Code / Codex CLI のプロンプト履歴を fzf で選択して Zed に貼り付ける。
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
mod debug_log;
mod guard;
mod history;
mod injector;
mod picker;
mod secure_log;
mod tmp_paths;

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
/// - `Direct(var)`: 本ツール専用の明示 override。`var` の値をそのままファイル
///   パスとして使う（例: `CLAUDE_HISTORY_PATH`）。
/// - `DirJoinFile(var, file_name)`: 他ツール自身の設定ディレクトリを間借りする
///   （例: `CODEX_HOME` は Codex CLI が history.jsonl の保存先として尊重する
///   環境変数）。
enum EnvPathSource {
    Direct(&'static str),
    DirJoinFile(&'static str, &'static str),
}

/// 解決されたパスの由来。`load_source` のメッセージ選択、および
/// `had_unexpected_error`（`main` の exit code 分岐に使う）の判定に使う。
/// どの由来でも `load_source` 自体は他ソースを巻き込まず常に継続する
/// （= プロセスをその場で終了させることはない）が、`ExplicitEnvVar` の
/// 欠落は `had_unexpected_error = true` として `main` に伝わり、両ソースとも
/// 0 件になった際の exit code に間接的に影響する。詳細は `load_source` 参照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathOrigin {
    /// デフォルトパス（環境変数なし）。欠落は「そのツールを使っていない」ことを示す。
    Default,
    /// 本ツール専用の明示 override 環境変数（`EnvPathSource::Direct`）。
    /// ユーザーが agent-history-pick に対して能動的に指定した値なので、
    /// 欠落は設定ミスの可能性が高いことを明示するメッセージを出す。
    ExplicitEnvVar(&'static str),
    /// 他ツール自身の環境変数を間借り（`EnvPathSource::DirJoinFile`）。
    /// `history.persistence = "none"` 等その他ツール側の設定次第で history.jsonl
    /// が存在しない状態も正当にありうるため fatal 相当のメッセージにはしないが、
    /// 「どの環境変数を参照したか」は明示し、設定ミスにも気づけるようにする。
    BorrowedEnvVar(&'static str),
}

/// `sources` を先頭から順に調べ、最初に設定されている環境変数からパスを解決する。
/// どれも未設定なら `$HOME/{default_relative}` にフォールバックする。
///
/// ソース別の `resolve_*_history_path` はこの関数に候補リストを渡すだけの薄い
/// ラッパーにすることで、探索ロジック自体の重複（≒ 将来ソースを追加するたびに
/// 同じ if-let チェーンを書き写すリスク）を避ける。
fn resolve_history_path(
    sources: &[EnvPathSource],
    default_relative: &str,
) -> (PathBuf, PathOrigin) {
    for source in sources {
        match source {
            EnvPathSource::Direct(var) => {
                if let Ok(p) = std::env::var(var) {
                    return (PathBuf::from(p), PathOrigin::ExplicitEnvVar(var));
                }
            }
            EnvPathSource::DirJoinFile(var, file_name) => {
                // 空文字列・空白のみ（例: `export CODEX_HOME=` や
                // `export CODEX_HOME=" "` のようなシェル設定ミス）は未設定として
                // 扱う。素通しすると CWD 相対の "history.jsonl" になり、
                // HOME-relative 前提（claude_history_path_is_home_relative_
                // not_cwd_relative と同じ保証）が崩れて意図しないファイルを
                // 拾いかねない。Direct はユーザーの明示指定なので空文字列も
                // そのまま尊重するが、DirJoinFile は他ツールの設定を間借りする
                // 性質上、空値（trim 後に空になる値を含む）は「値なし」として
                // 扱うのが安全側。判定に使った trim 後の値をそのままパス構築にも
                // 使う（判定と構築で別の文字列を使うと、前後に空白が付いた
                // 有効なパス — 例: direnv 由来の `" /Users/alice/.codex"` — が
                // untrimmed のまま存在しないパスとして扱われてしまう）。
                if let Ok(dir) = std::env::var(var) {
                    let trimmed = dir.trim();
                    if !trimmed.is_empty() {
                        return (
                            PathBuf::from(trimmed).join(file_name),
                            PathOrigin::BorrowedEnvVar(var),
                        );
                    }
                }
            }
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    (
        PathBuf::from(home).join(default_relative),
        PathOrigin::Default,
    )
}

/// Claude Code の履歴ファイルのパスを解決する。
/// `CLAUDE_HISTORY_PATH` 環境変数が設定されていればそのパスを使う。
/// 未設定なら `$HOME/.claude/history.jsonl` を返す。
fn resolve_claude_history_path() -> (PathBuf, PathOrigin) {
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
fn resolve_codex_history_path() -> (PathBuf, PathOrigin) {
    resolve_history_path(
        &[
            EnvPathSource::Direct("CODEX_HISTORY_PATH"),
            EnvPathSource::DirJoinFile("CODEX_HOME", "history.jsonl"),
        ],
        ".codex/history.jsonl",
    )
}

/// 1 ソース分の履歴を読み込む。エラー時は理由を stderr に出しつつ常に空
/// `Vec` を返して継続する（他ソースの読み込み結果を道連れにしない）。
///
/// - `NotFound`: `origin` に応じて 3 パターンのメッセージを出し分ける。
///   `Default`（そのツールを使っていない環境）・`BorrowedEnvVar`（他ツール側の
///   設定次第で正当にありうる）は `had_unexpected_error = false`。
///   `ExplicitEnvVar`（本ツール専用の明示 override）は、ユーザーが能動的に
///   指定したパスが存在しないという設定ミス以外の理由が考えにくいため、
///   `NotFound` であっても `had_unexpected_error = true` にする。
/// - それ以外の IO エラー（Permission denied・EIO 等）: `origin` によらず
///   環境異常の可能性が高いため `had_unexpected_error = true`。
///
/// いずれも fatal でプロセスを終了しないのは、片方のソースの異常（例:
/// `~/.codex/` のパーミッション異常）で、既にロード成功した他方のソースの
/// 履歴まで失って picker が開かなくなるのを避けるため。ただし「両ソースとも
/// 0 件」になった場合、正当な「未使用」と本物の異常を exit code で区別
/// できないと、異常発生時に気づく手段がなくなる。そのため `had_unexpected_error`
/// を呼び出し元に伝え、`main` が両ソース 0 件時の exit code をこれで分岐する。
fn load_source<F: FnOnce(&Path) -> io::Result<Vec<Prompt>>>(
    name: &str,
    path: &Path,
    origin: PathOrigin,
    loader: F,
) -> (Vec<Prompt>, bool) {
    match loader(path) {
        Ok(v) => (v, false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let had_unexpected_error = match origin {
                PathOrigin::Default => {
                    eprintln!(
                        "[{name}] 履歴ファイルが見つからないためスキップします（{}）",
                        path.display()
                    );
                    false
                }
                PathOrigin::ExplicitEnvVar(var) => {
                    eprintln!(
                        "[{name}] {var} で指定したパスが見つかりません: {}",
                        path.display()
                    );
                    true
                }
                PathOrigin::BorrowedEnvVar(var) => {
                    eprintln!(
                        "[{name}] {var} を参照しましたが履歴ファイルが見つかりませんでした（{}）。\
                         {var} 側で履歴保存が無効化されている場合はこのまま無視して構いません。",
                        path.display()
                    );
                    false
                }
            };
            (Vec::new(), had_unexpected_error)
        }
        Err(e) => {
            eprintln!("[{name}] 履歴の読み込みに失敗: {}", path.display());
            eprintln!("エラー: {e}");
            (Vec::new(), true)
        }
    }
}

fn main() {
    guard::acquire();

    let (claude_path, claude_origin) = resolve_claude_history_path();
    let (codex_path, codex_origin) = resolve_codex_history_path();

    let (claude_prompts, claude_had_error) = load_source(
        history::Source::Claude.label(),
        &claude_path,
        claude_origin,
        claude::load_claude_prompts,
    );
    let (codex_prompts, codex_had_error) = load_source(
        history::Source::Codex.label(),
        &codex_path,
        codex_origin,
        codex::load_codex_prompts,
    );
    let had_unexpected_error = claude_had_error || codex_had_error;

    let mut all_prompts = claude_prompts;
    all_prompts.extend(codex_prompts);
    let prompts = history::merge_sort_dedup(all_prompts);
    debug_log::log_startup(&prompts);

    if prompts.is_empty() {
        eprintln!("履歴が見つかりませんでした");
        guard::release();
        // 両ソースとも「単に未使用」なら exit(0)。1 つでも Permission denied・EIO
        // 等の本物の IO 異常があったなら exit(1) にし、正常な空とは exit code で
        // 区別できるようにする（stderr のメッセージを見逃しても気づけるように）。
        std::process::exit(if had_unexpected_error { 1 } else { 0 });
    }

    let selected = match picker::pick(&prompts, had_unexpected_error) {
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

    /// `load_source` のテストで「実際には open されない・値そのものに意味がない」
    /// パス引数として使い回す（loader クロージャがパスを無視するケース向け）。
    const UNUSED_PATH: &str = "/unused";

    /// `load_source` のテストで NotFound エラーを発生させるための、
    /// 存在しないことが確実なパス。
    const NONEXISTENT_HISTORY_PATH: &str = "/definitely/does/not/exist/history.jsonl";

    #[test]
    fn claude_env_var_set_returns_custom_path_and_explicit_origin() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_all_history_env_vars();
        std::env::set_var("CLAUDE_HISTORY_PATH", "/custom/path/history.jsonl");
        let (path, origin) = resolve_claude_history_path();
        clear_all_history_env_vars();
        assert_eq!(path, PathBuf::from("/custom/path/history.jsonl"));
        assert_eq!(origin, PathOrigin::ExplicitEnvVar("CLAUDE_HISTORY_PATH"));
    }

    #[test]
    fn claude_env_var_unset_returns_default_path_and_origin() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_all_history_env_vars();
        let (path, origin) = resolve_claude_history_path();
        assert!(path.to_string_lossy().ends_with(".claude/history.jsonl"));
        assert_eq!(origin, PathOrigin::Default);
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
        let (path, origin) = resolve_claude_history_path();
        match original_home {
            Some(h) => std::env::set_var("HOME", h),
            None => std::env::remove_var("HOME"),
        }
        assert_eq!(path, PathBuf::from("./.claude/history.jsonl"));
        assert_eq!(origin, PathOrigin::Default);
    }

    #[test]
    fn claude_env_var_empty_string_is_accepted_as_env_var_path() {
        // Direct はユーザーの明示指定なので、DirJoinFile（CODEX_HOME）と違い
        // 空文字列も「値なし」に変換せずそのまま尊重する。
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_all_history_env_vars();
        std::env::set_var("CLAUDE_HISTORY_PATH", "");
        let (path, origin) = resolve_claude_history_path();
        clear_all_history_env_vars();
        assert_eq!(path, PathBuf::from(""));
        assert_eq!(origin, PathOrigin::ExplicitEnvVar("CLAUDE_HISTORY_PATH"));
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
        let (path, origin) = resolve_codex_history_path();
        clear_all_history_env_vars();
        assert_eq!(path, PathBuf::from("/custom/codex-history.jsonl"));
        assert_eq!(origin, PathOrigin::ExplicitEnvVar("CODEX_HISTORY_PATH"));
    }

    #[test]
    fn codex_home_is_used_when_history_path_unset() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_all_history_env_vars();
        std::env::set_var("CODEX_HOME", "/fake/codex-home");
        let (path, origin) = resolve_codex_history_path();
        clear_all_history_env_vars();
        assert_eq!(path, PathBuf::from("/fake/codex-home/history.jsonl"));
        assert_eq!(
            origin,
            PathOrigin::BorrowedEnvVar("CODEX_HOME"),
            "CODEX_HOME は Codex 自身の設定を間借りしているだけで agent-history-pick \
             への明示指定ではないため BorrowedEnvVar にすべき"
        );
    }

    #[test]
    fn codex_home_empty_string_is_treated_as_unset() {
        // `export CODEX_HOME=` のようなシェル設定ミスで空文字列になった場合、
        // CWD 相対の "history.jsonl" を作ってしまうと HOME-relative 前提が崩れる。
        // 空値は未設定として扱い、デフォルトパスにフォールバックすべき。
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_all_history_env_vars();
        std::env::set_var("CODEX_HOME", "");
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let (path, origin) = resolve_codex_history_path();
        clear_all_history_env_vars();
        assert_eq!(path, PathBuf::from(&home).join(".codex/history.jsonl"));
        assert_eq!(origin, PathOrigin::Default);
    }

    #[test]
    fn codex_home_padded_with_whitespace_uses_trimmed_path() {
        // direnv や templated env file 等で前後に空白が付いた有効な値
        // （空白のみではない）は、trim した結果をパス構築に使うべき。
        // 判定にだけ trim を使い構築は untrimmed のままだと、有効なパスが
        // 存在しないパスとして扱われてしまう。
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_all_history_env_vars();
        std::env::set_var("CODEX_HOME", "  /fake/codex-home  ");
        let (path, origin) = resolve_codex_history_path();
        clear_all_history_env_vars();
        assert_eq!(path, PathBuf::from("/fake/codex-home/history.jsonl"));
        assert_eq!(origin, PathOrigin::BorrowedEnvVar("CODEX_HOME"));
    }

    #[test]
    fn codex_home_whitespace_only_is_treated_as_unset() {
        // `export CODEX_HOME=" "` のような空白のみの値も、素通しすると
        // CWD 相対の "history.jsonl" を作ってしまう（is_empty() だけでは
        // 検知できない）。trim 後に空になる値も未設定として扱うべき。
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_all_history_env_vars();
        std::env::set_var("CODEX_HOME", "   ");
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let (path, origin) = resolve_codex_history_path();
        clear_all_history_env_vars();
        assert_eq!(path, PathBuf::from(&home).join(".codex/history.jsonl"));
        assert_eq!(origin, PathOrigin::Default);
    }

    #[test]
    fn codex_home_missing_history_file_is_treated_as_non_fatal() {
        // history.persistence = "none" 等で Codex 側が history.jsonl を意図的に
        // 作らない設定も正当にありうる。CODEX_HOME 経由の欠落で agent-history-pick
        // 自体が fatal error になってはならない（Claude 側のみで動作継続すべき）。
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_all_history_env_vars();
        std::env::set_var("CODEX_HOME", "/definitely/does/not/exist/codex-home");
        let (path, origin) = resolve_codex_history_path();
        clear_all_history_env_vars();
        let (result, had_error) = load_source("Codex", &path, origin, codex::load_codex_prompts);
        assert!(
            result.is_empty(),
            "CODEX_HOME 由来の欠落は空 Vec を返して継続すべき（fatal にならない）"
        );
        assert!(
            !had_error,
            "NotFound は正当な理由でありうるため had_unexpected_error は false であるべき"
        );
    }

    #[test]
    fn codex_default_path_is_under_home_when_no_env_vars() {
        let _guard = ENV_MUTEX.lock().unwrap();
        clear_all_history_env_vars();
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let (path, origin) = resolve_codex_history_path();
        assert_eq!(path, PathBuf::from(&home).join(".codex/history.jsonl"));
        assert_eq!(origin, PathOrigin::Default);
    }

    // ─── load_source ────────────────────────────────────────────────────────

    #[test]
    fn load_source_returns_loader_result_on_success() {
        let (result, had_error) =
            load_source("Test", Path::new(UNUSED_PATH), PathOrigin::Default, |_| {
                Ok(vec![history::test_support::make_prompt(
                    history::Source::Claude,
                    "ok",
                    Some(100),
                )])
            });
        assert_eq!(result.len(), 1);
        assert!(!had_error);
    }

    #[test]
    fn load_source_default_path_not_found_returns_empty_without_exit() {
        // デフォルトパス（PathOrigin::Default）で NotFound は非致命: 空 Vec を返し継続する。
        let (result, had_error) = load_source(
            "Test",
            Path::new(NONEXISTENT_HISTORY_PATH),
            PathOrigin::Default,
            claude::load_claude_prompts,
        );
        assert!(result.is_empty());
        assert!(
            !had_error,
            "NotFound は正当な理由なので had_unexpected_error は false"
        );
    }

    #[test]
    fn load_source_explicit_env_var_not_found_is_non_fatal_but_flagged() {
        // ExplicitEnvVar 由来の NotFound はプロセスを終了しない（他ソースを
        // 道連れにしない）。ただし Default/BorrowedEnvVar と異なり、ユーザーが
        // 能動的に指定したパスの欠落は設定ミス以外に理由が考えにくいため
        // had_unexpected_error = true にし、main の exit code 分岐に伝える。
        let (result, had_error) = load_source(
            "Test",
            Path::new(NONEXISTENT_HISTORY_PATH),
            PathOrigin::ExplicitEnvVar("TEST_HISTORY_PATH"),
            claude::load_claude_prompts,
        );
        assert!(result.is_empty());
        assert!(
            had_error,
            "ExplicitEnvVar の NotFound は設定ミスの可能性が高いため had_unexpected_error = true にすべき"
        );
    }

    #[test]
    fn load_source_borrowed_env_var_not_found_is_non_fatal() {
        let (result, had_error) = load_source(
            "Test",
            Path::new(NONEXISTENT_HISTORY_PATH),
            PathOrigin::BorrowedEnvVar("TEST_HOME"),
            claude::load_claude_prompts,
        );
        assert!(result.is_empty());
        assert!(!had_error);
    }

    #[test]
    fn load_source_permission_style_error_does_not_abort_other_source() {
        // NotFound 以外の IO エラー（Permission denied 相当）でも load_source
        // 自体はプロセスを終了せず空 Vec を返す。呼び出し元（main）が他ソースの
        // 結果を握りつぶさないことを型レベルで保証する（実際の Permission denied
        // 発生はサンドボックス依存のため、io::Error::other で同じ
        // 「NotFound 以外」分岐を代表させる）。
        let (result, had_error) =
            load_source("Test", Path::new(UNUSED_PATH), PathOrigin::Default, |_| {
                Err(io::Error::other("simulated permission denied"))
            });
        assert!(result.is_empty());
        assert!(
            had_error,
            "NotFound 以外の IO エラーは main の exit code 分岐のため had_unexpected_error = true"
        );
    }

    #[test]
    fn load_source_had_error_true_lets_main_distinguish_real_failure_from_empty_history() {
        // 両ソースとも IO エラー（Permission denied 等）で失敗した場合、main は
        // 「単に履歴がない」場合と同じ exit(0) にせず exit(1) にすべき。main() 自体は
        // process::exit するためユニットテスト不可能なので、その分岐条件となる
        // had_unexpected_error の OR 結合が両方 true を伝播することをここで検証する。
        let (_, claude_had_error) = load_source(
            "Claude",
            Path::new(UNUSED_PATH),
            PathOrigin::Default,
            |_| Err(io::Error::other("simulated permission denied")),
        );
        let (_, codex_had_error) =
            load_source("Codex", Path::new(UNUSED_PATH), PathOrigin::Default, |_| {
                Err(io::Error::other("simulated EIO"))
            });
        assert!(claude_had_error && codex_had_error);
    }
}
