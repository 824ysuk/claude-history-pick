//! fzf 起動時の統合結果・選択確定内容を記録するデバッグログ層。
//!
//! 責務: ログファイルへの追記のみ。フォーマット組み立て（純粋関数）と
//! ファイル I/O を分離しているのは、フォーマット部分を実ファイルなしで
//! テストするため（injector.rs の build_script_args パターンを踏襲）。

use crate::history::Prompt;
use chrono::Local;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

/// STARTUP ログに残す上位件数。
const STARTUP_LOG_LIMIT: usize = 10;

/// デバッグログファイルパス。
///
/// UID を含めることでマルチユーザー環境での衝突を防ぐ（injector.rs の
/// osascript_log_path と同パターン）。
fn debug_log_path() -> PathBuf {
    let uid = nix::unistd::getuid();
    PathBuf::from(format!("/tmp/{uid}.agent-history-pick.debug.log"))
}

/// ログの 1 行に改行が混入して行境界が壊れないよう単一行化する。
fn single_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

/// STARTUP ログ行を組み立てる（純粋関数）。`prompts` は merge_sort_dedup 適用済み
/// である前提で、先頭から `STARTUP_LOG_LIMIT` 件のみを対象にする。
fn format_startup_lines(prompts: &[Prompt]) -> Vec<String> {
    let now = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    prompts
        .iter()
        .take(STARTUP_LOG_LIMIT)
        .enumerate()
        .map(|(i, p)| {
            format!(
                "[{now}] STARTUP #{} [{}] {} ({})",
                i + 1,
                p.source().label(),
                single_line(p.display()),
                p.timestamp().unwrap_or("")
            )
        })
        .collect()
}

/// SELECTED ログ行を組み立てる（純粋関数）。
fn format_selection_line(idx: usize, prompt: &Prompt) -> String {
    let now = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    format!(
        "[{now}] SELECTED idx={} [{}] {} ({})",
        idx,
        prompt.source().label(),
        single_line(prompt.display()),
        prompt.timestamp().unwrap_or("")
    )
}

/// `lines` をデバッグログファイルに追記する。open 失敗時はサイレントに何もしない
/// （デバッグ用途のログであり、本処理を止める理由にはならない）。
fn append_lines(lines: &[String]) {
    let Ok(mut file) = OpenOptions::new()
        .append(true)
        .create(true)
        .open(debug_log_path())
    else {
        return;
    };
    for line in lines {
        let _ = writeln!(file, "{line}");
    }
}

/// merge_sort_dedup 後の統合結果（上位 `STARTUP_LOG_LIMIT` 件）をデバッグログへ追記する。
pub fn log_startup(prompts: &[Prompt]) {
    append_lines(&format_startup_lines(prompts));
}

/// fzf で選択が確定した直後に、選ばれた内容をデバッグログへ追記する。
pub fn log_selection(idx: usize, prompt: &Prompt) {
    append_lines(&[format_selection_line(idx, prompt)]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::test_support::make_prompt;
    use crate::history::Source;

    #[test]
    fn startup_lines_are_capped_at_limit() {
        let prompts: Vec<Prompt> = (0..15)
            .map(|i| make_prompt(Source::Claude, &format!("prompt {i}"), Some(i as i64)))
            .collect();
        let lines = format_startup_lines(&prompts);
        assert_eq!(lines.len(), STARTUP_LOG_LIMIT);
    }

    #[test]
    fn startup_lines_include_rank_source_display_and_timestamp() {
        let prompts = vec![make_prompt(Source::Codex, "hello", Some(0))];
        let lines = format_startup_lines(&prompts);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("STARTUP #1"));
        assert!(lines[0].contains("[Codex]"));
        assert!(lines[0].contains("hello"));
        assert!(lines[0].starts_with('['));
    }

    #[test]
    fn startup_lines_empty_input_returns_empty() {
        assert!(format_startup_lines(&[]).is_empty());
    }

    #[test]
    fn startup_line_display_is_single_lined() {
        let prompts = vec![make_prompt(Source::Claude, "first\nsecond", Some(0))];
        let lines = format_startup_lines(&prompts);
        assert!(!lines[0].contains('\n'));
        assert!(lines[0].contains("first second"));
    }

    #[test]
    fn selection_line_includes_idx_source_and_display() {
        let prompt = make_prompt(Source::Claude, "selected text", Some(0));
        let line = format_selection_line(3, &prompt);
        assert!(line.contains("SELECTED idx=3"));
        assert!(line.contains("[Claude]"));
        assert!(line.contains("selected text"));
    }

    #[test]
    fn selection_line_missing_timestamp_renders_empty_parens() {
        let prompt = make_prompt(Source::Claude, "no ts", None);
        let line = format_selection_line(0, &prompt);
        assert!(line.ends_with("()"));
    }

    /// `log_startup` / `log_selection` が実際にディスク上のログファイルへ
    /// 追記することを確認する（フォーマット純粋関数のテストでは open/write の
    /// 実際の I/O 経路は検証できないため）。injector.rs の
    /// `spawn_injector_with_existing_program_returns_ok` と同様、実 /tmp パスへの
    /// 副作用を許容するテスト。
    #[test]
    fn log_startup_and_log_selection_write_to_real_debug_log_file() {
        let marker = "debug-log-real-io-check-9f3c2a";
        let prompts = vec![make_prompt(Source::Claude, marker, Some(0))];

        log_startup(&prompts);
        log_selection(0, &prompts[0]);

        let contents = std::fs::read_to_string(debug_log_path()).expect("デバッグログの読み込みに失敗");
        assert!(
            contents.contains(&format!("STARTUP #1 [Claude] {marker}")),
            "STARTUP 行が実ファイルに書き込まれていない: {contents}"
        );
        assert!(
            contents.contains(&format!("SELECTED idx=0 [Claude] {marker}")),
            "SELECTED 行が実ファイルに書き込まれていない: {contents}"
        );
    }
}
