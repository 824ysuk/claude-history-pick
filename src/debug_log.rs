//! fzf 起動時の統合結果・選択確定内容を記録するデバッグログ層。
//!
//! 責務: ログファイルへの追記のみ。フォーマット組み立て（純粋関数）と
//! ファイル I/O を分離しているのは、フォーマット部分を実ファイルなしで
//! テストするため（injector.rs の build_script_args パターンを踏襲）。
//!
//! ## サイズ上限とローテーション
//!
//! `/tmp/{uid}.agent-history-pick.debug.log` は起動のたびに無条件で追記される
//! ため、ローテーションなしでは中長期運用で際限なく肥大化する。log4rs の
//! RollingFileAppender（サイズ閾値 + 固定世代ローテーション）と同じ型を、この
//! ログの実際の書き込み量（起動 1 回あたり最大 11 行）に合わせて縮小適用する:
//! 上限を超えたら `.1` へ 1 世代だけ退避し、新規ファイルから書き直す。
//!
//! ## symlink 攻撃対策
//!
//! ログパスは UID から決定的に導出されるため、マルチユーザー環境では他ユーザーが
//! 事前にこのパスへ symlink を仕込む競合（CWE-59 / TOCTOU）が理論上成立する。
//! `open(2)` に `O_NOFOLLOW` を指定し、symlink 経由のオープンを OS レベルで拒否する
//! （Rust std: `OpenOptionsExt::custom_flags`）。ローテーション判定も
//! `symlink_metadata`（lstat 相当）で行い、symlink を追跡しない。

use crate::history::Prompt;
use chrono::Local;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

/// STARTUP ログに残す上位件数。
const STARTUP_LOG_LIMIT: usize = 10;

/// ログ 1 世代あたりの上限バイト数。
///
/// 1 回の起動で最大 11 行（STARTUP 10 件 + SELECTED 1 件）× 平均 150 バイト
/// ≈ 1.65KB のため、1MB は数百回分の実行履歴に相当し、個人利用のデバッグ用途
/// として十分な保持期間になる（環境依存ではなく本ログの用途規模から導いた設計値）。
const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// デバッグログファイルパス。
///
/// UID を含めることでマルチユーザー環境での衝突を防ぐ（injector.rs の
/// osascript_log_path と同パターン）。
fn debug_log_path() -> PathBuf {
    let uid = nix::unistd::getuid();
    PathBuf::from(format!("/tmp/{uid}.agent-history-pick.debug.log"))
}

/// ローテーション先のパス（`.1` を 1 世代だけ保持）。
fn rotated_log_path(path: &Path) -> PathBuf {
    let mut rotated = path.as_os_str().to_os_string();
    rotated.push(".1");
    PathBuf::from(rotated)
}

/// 現在のファイルサイズがローテーション閾値を超えているか（純粋関数）。
fn should_rotate(current_size_bytes: u64) -> bool {
    current_size_bytes >= MAX_LOG_BYTES
}

/// `path` が上限サイズを超えていれば `.1` へ 1 世代だけ退避する。
///
/// symlink 攻撃対策として `symlink_metadata`（lstat 相当、symlink を追跡しない）
/// でサイズを確認する。symlink 等の通常ファイルでない実体はここでは何もせず、
/// 後続の `append_lines_to` の `O_NOFOLLOW` open に判断を委ねる（そこで安全に失敗する）。
fn rotate_if_needed(path: &Path) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if meta.is_file() && should_rotate(meta.len()) {
        let _ = std::fs::rename(path, rotated_log_path(path));
    }
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

/// `lines` を `path` に追記する（`append_lines` の本体、path を差し替えられるよう
/// 分離しているのはローテーション・symlink 拒否を実ファイルなしでテストするため）。
///
/// 上限超過時はローテーションしてから、`O_NOFOLLOW` 付きで open する。
/// open 失敗時（symlink 経由の拒否を含む）はサイレントに何もしない
/// （デバッグ用途のログであり、本処理を止める理由にはならない）。
fn append_lines_to(path: &Path, lines: &[String]) {
    if lines.is_empty() {
        return;
    }
    rotate_if_needed(path);
    let Ok(mut file) = OpenOptions::new()
        .append(true)
        .create(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    else {
        return;
    };
    for line in lines {
        let _ = writeln!(file, "{line}");
    }
}

fn append_lines(lines: &[String]) {
    append_lines_to(&debug_log_path(), lines);
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
    use std::os::unix::fs::symlink;

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

    #[test]
    fn should_rotate_below_threshold_is_false() {
        assert!(!should_rotate(MAX_LOG_BYTES - 1));
    }

    #[test]
    fn should_rotate_at_or_above_threshold_is_true() {
        assert!(should_rotate(MAX_LOG_BYTES));
        assert!(should_rotate(MAX_LOG_BYTES + 1));
    }

    #[test]
    fn rotated_log_path_appends_dot_one() {
        let path = PathBuf::from("/tmp/123.agent-history-pick.debug.log");
        assert_eq!(
            rotated_log_path(&path),
            PathBuf::from("/tmp/123.agent-history-pick.debug.log.1")
        );
    }

    #[test]
    fn append_lines_to_creates_file_and_appends() {
        let dir = tempfile::tempdir().expect("tempdir 作成に失敗");
        let path = dir.path().join("debug.log");

        append_lines_to(&path, &["line 1".to_string()]);
        append_lines_to(&path, &["line 2".to_string()]);

        let contents = std::fs::read_to_string(&path).expect("読み込みに失敗");
        assert_eq!(contents, "line 1\nline 2\n");
    }

    #[test]
    fn append_lines_to_empty_lines_does_not_create_file() {
        let dir = tempfile::tempdir().expect("tempdir 作成に失敗");
        let path = dir.path().join("debug.log");

        append_lines_to(&path, &[]);

        assert!(!path.exists(), "空行では何も書き込まれないはず");
    }

    #[test]
    fn append_lines_to_rotates_when_threshold_exceeded() {
        let dir = tempfile::tempdir().expect("tempdir 作成に失敗");
        let path = dir.path().join("debug.log");

        std::fs::write(&path, "x".repeat(MAX_LOG_BYTES as usize)).expect("下準備の書き込みに失敗");

        append_lines_to(&path, &["new entry".to_string()]);

        let rotated = rotated_log_path(&path);
        assert!(rotated.exists(), "閾値超過時に .1 へ退避されていない");
        assert_eq!(
            std::fs::read_to_string(&rotated).expect("rotated 読み込みに失敗").len(),
            MAX_LOG_BYTES as usize,
            "退避先に旧内容が残っていない"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("読み込みに失敗"),
            "new entry\n",
            "ローテーション後の新規ファイルに新しい行だけが入っていない"
        );
    }

    #[test]
    fn append_lines_to_below_threshold_does_not_rotate() {
        let dir = tempfile::tempdir().expect("tempdir 作成に失敗");
        let path = dir.path().join("debug.log");
        std::fs::write(&path, "small content").expect("下準備の書き込みに失敗");

        append_lines_to(&path, &["new entry".to_string()]);

        assert!(
            !rotated_log_path(&path).exists(),
            "閾値未満なのに .1 が作られている"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("読み込みに失敗"),
            "small contentnew entry\n"
        );
    }

    /// symlink 経由の open が `O_NOFOLLOW` により拒否され、symlink の指す先の
    /// ファイルが書き換えられないことを確認する（CWE-59 対策の実効性テスト）。
    #[test]
    fn append_lines_to_refuses_to_follow_symlink() {
        let dir = tempfile::tempdir().expect("tempdir 作成に失敗");
        let victim = dir.path().join("victim.txt");
        let log_path = dir.path().join("debug.log");
        std::fs::write(&victim, "original victim content").expect("victim 作成に失敗");
        symlink(&victim, &log_path).expect("symlink 作成に失敗");

        append_lines_to(&log_path, &["attacker-controlled line".to_string()]);

        let victim_contents = std::fs::read_to_string(&victim).expect("victim 読み込みに失敗");
        assert_eq!(
            victim_contents, "original victim content",
            "symlink 経由で victim ファイルが書き換えられてしまった"
        );
    }
}
