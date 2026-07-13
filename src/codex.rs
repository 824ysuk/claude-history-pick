//! Codex CLI の ~/.codex/history.jsonl からプロンプト履歴を読み込む層。
//!
//! 責務: JSON パースのみ。フィルタリング・重複除去・他ソースとのマージは
//! `history::merge_sort_dedup` が担う。UI（fzf）・クリップボード・キーストロークは扱わない。
//!
//! フォーマット（openai/codex `codex-rs/message-history/src/lib.rs` で一次確認済み）:
//! `{"session_id":"<uuid>","ts":<unix_seconds>,"text":"<message>"}`
//! Claude 側の `timestamp`（ミリ秒）と異なり `ts` は秒単位。

use crate::history::{read_lines_from_path, unix_ms_to_local_iso, Prompt, Source};
use serde::Deserialize;
use std::path::Path;

/// history.jsonl の 1 行に対応する構造体。
#[derive(Deserialize)]
struct CodexHistoryEntry {
    ts: u64,
    text: Option<String>,
}

/// `history_path` の JSONL を読み込み、`Prompt` 一覧を返す（フィルタ・重複除去前）。
///
/// JSON パース失敗行はスキップし、ファイル全体の読み込みは続行する（Claude 側と同じ方針）。
/// 行読み込み中の IO エラーは呼び出し元に伝播する。
pub fn load_codex_prompts(history_path: &Path) -> std::io::Result<Vec<Prompt>> {
    let lines = read_lines_from_path(history_path)?;
    Ok(collect_prompts(lines.into_iter()))
}

/// JSONL 1 行から `Prompt` を取り出す。
///
/// 空行・JSON パース失敗・`text` 欠落・空文字列はすべて `None` を返す。
fn parse_entry(line: &str) -> Option<Prompt> {
    if line.trim().is_empty() {
        return None;
    }
    let entry: CodexHistoryEntry = serde_json::from_str(line).ok()?;
    let display = entry.text?.trim().to_string();
    if display.is_empty() {
        return None;
    }
    // ts は Unix 秒。Claude 側との統一表現（ミリ秒）に変換する。
    let timestamp_ms = (entry.ts as i64).saturating_mul(1000);
    Some(Prompt {
        source: Source::Codex,
        display: display.clone(),
        // Codex には pastedContents 相当の仕組みがないため display == full_text。
        full_text: display,
        iso_timestamp: Some(unix_ms_to_local_iso(timestamp_ms)),
        timestamp_ms: Some(timestamp_ms),
    })
}

/// JSONL 行イテレータからプロンプトを収集する。フィルタ・重複除去は行わない。
fn collect_prompts(lines: impl Iterator<Item = String>) -> Vec<Prompt> {
    lines.filter_map(|l| parse_entry(&l)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    fn lines(raw: &str) -> impl Iterator<Item = String> + '_ {
        raw.lines().map(|l| l.to_string())
    }

    fn displays(prompts: Vec<Prompt>) -> Vec<String> {
        prompts.into_iter().map(|p| p.display).collect()
    }

    #[test]
    fn load_codex_prompts_propagates_not_found() {
        // read_lines 自体の IO エラー伝播は history.rs で確認済み。ここでは
        // load_codex_prompts が正しく read_lines_from_path に繋がっていることのみ確認する。
        let result = load_codex_prompts(Path::new("/definitely/does/not/exist/history.jsonl"));
        let err = result.expect_err("存在しないパスは Err を返すべき");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn normal_entry_is_included() {
        let input = r#"{"session_id":"abc","ts":1778904379,"text":"やっほ"}"#;
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["やっほ"]);
    }

    #[test]
    fn entry_source_is_codex() {
        let input = r#"{"session_id":"abc","ts":1778904379,"text":"やっほ"}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result[0].source, Source::Codex);
    }

    #[test]
    fn seconds_timestamp_is_converted_to_milliseconds() {
        let input = r#"{"session_id":"abc","ts":1778904379,"text":"やっほ"}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result[0].timestamp_ms, Some(1_778_904_379_000));
    }

    #[test]
    fn iso_timestamp_is_derived_from_ts() {
        // 1778904379 秒 ≈ 2026 年（実データのオーダー）
        let input = r#"{"session_id":"abc","ts":1778904379,"text":"やっほ"}"#;
        let result = collect_prompts(lines(input));
        let ts = result[0].iso_timestamp.as_deref().unwrap();
        assert!(
            ts.contains("2026"),
            "2026 年のタイムスタンプになるべき: {ts}"
        );
    }

    #[test]
    fn display_equals_full_text() {
        // Codex には pastedContents 相当の概念がない。
        let input = r#"{"session_id":"abc","ts":100,"text":"複数行\nテキスト"}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result[0].display, result[0].full_text);
        assert_eq!(result[0].display, "複数行\nテキスト");
    }

    #[test]
    fn empty_text_is_excluded() {
        let input = r#"{"session_id":"abc","ts":100,"text":""}"#;
        let result = collect_prompts(lines(input));
        assert!(result.is_empty());
    }

    #[test]
    fn missing_text_field_is_skipped() {
        let input = r#"{"session_id":"abc","ts":100}
{"session_id":"abc","ts":200,"text":"有効"}"#;
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["有効"]);
    }

    #[test]
    fn invalid_json_line_is_skipped() {
        let input = "not-json\n{\"session_id\":\"abc\",\"ts\":100,\"text\":\"有効\"}";
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["有効"]);
    }

    #[test]
    fn whitespace_is_trimmed() {
        let input = r#"{"session_id":"abc","ts":100,"text":"  前後スペース  "}"#;
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["前後スペース"]);
    }

    #[test]
    fn empty_input_returns_empty_vec() {
        let result = collect_prompts(lines(""));
        assert!(result.is_empty());
    }

    #[test]
    fn multiline_text_is_preserved() {
        // 実データに改行を含む text が存在することを確認済み（エラーメッセージ引用等）。
        let input = r#"{"session_id":"abc","ts":100,"text":"line1\nline2"}"#;
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["line1\nline2"]);
    }

    #[test]
    fn multiple_entries_from_same_session_are_all_visible() {
        let input = r#"{"session_id":"s1","ts":100,"text":"1st"}
{"session_id":"s1","ts":200,"text":"2nd"}
{"session_id":"s2","ts":300,"text":"3rd"}"#;
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["1st", "2nd", "3rd"]);
    }

    // ─── 実 history.jsonl との統合テスト ─────────────────────────────────────

    #[test]
    fn real_history_file_is_readable_and_contains_entries() {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let path = std::path::PathBuf::from(&home).join(".codex/history.jsonl");
        if !path.exists() {
            eprintln!(
                "SKIP: {} が存在しないため統合テストをスキップ",
                path.display()
            );
            return;
        }
        let result = load_codex_prompts(&path).expect("history.jsonl の読み込みに失敗");
        assert!(!result.is_empty(), "history.jsonl にエントリが存在すること");
        assert!(
            result.iter().all(|p| p.timestamp_ms.is_some()),
            "Codex エントリは常に timestamp_ms を持つべき"
        );
        assert!(
            result.iter().all(|p| p.source == Source::Codex),
            "全エントリの source が Codex であるべき"
        );
    }
}
