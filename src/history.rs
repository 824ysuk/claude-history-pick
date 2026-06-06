//! Claude Code の ~/.claude/history.jsonl からプロンプト履歴を読み込む層。
//!
//! 責務: JSON パース・フィルタリング・重複除去のみ。
//! UI（fzf）・クリップボード・キーストロークは扱わない。

use serde::Deserialize;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// history.jsonl の 1 行に対応する構造体。
/// `display` フィールドだけ取り出し、他フィールドは無視する。
#[derive(Deserialize)]
struct HistoryEntry {
    display: Option<String>,
}

/// `history_path` の JSONL を読み込み、表示用プロンプト一覧を返す。
///
/// フィルタ条件:
/// - `display` フィールドが存在し、空でない行のみ採用
/// - '/' 始まりのスラッシュコマンド（`/help` 等）を除外
/// - 重複エントリは先出順で除去（awk '!seen[$0]++' の Rust 等価）
///
/// パース失敗行はスキップし、ファイル全体の読み込みは続行する。
pub fn load_prompts(history_path: &Path) -> std::io::Result<Vec<String>> {
    let file = File::open(history_path)?;
    let reader = BufReader::new(file);
    Ok(collect_prompts(
        reader.lines().map(|l| l.unwrap_or_default()),
    ))
}

/// JSONL 行イテレータからプロンプトを収集する（テスト可能な純粋処理層）。
///
/// 行単位のパース失敗はスキップして続行する。ファイル I/O を伴わないため
/// 失敗しない。返り値を Result にしないことでその事実を型で表現する。
pub fn collect_prompts(lines: impl Iterator<Item = String>) -> Vec<String> {
    let mut prompts = Vec::new();
    let mut seen = HashSet::new();

    for line in lines {
        if line.trim().is_empty() {
            continue;
        }

        let entry: HistoryEntry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue,
        };

        if let Some(display) = entry.display {
            let display = display.trim().to_string();
            if display.is_empty() || display.starts_with('/') {
                continue;
            }
            if seen.insert(display.clone()) {
                prompts.push(display);
            }
        }
    }

    prompts.reverse();
    prompts
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(raw: &str) -> impl Iterator<Item = String> + '_ {
        raw.lines().map(|l| l.to_string())
    }

    #[test]
    fn normal_entry_is_included() {
        let input = r#"{"display":"ビルドして"}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result, vec!["ビルドして"]);
    }

    #[test]
    fn slash_command_is_excluded() {
        let input = r#"{"display":"/help"}
{"display":"通常のプロンプト"}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result, vec!["通常のプロンプト"]);
    }

    #[test]
    fn duplicate_is_removed_keeping_first() {
        let input = r#"{"display":"重複テスト"}
{"display":"重複テスト"}
{"display":"別のプロンプト"}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result, vec!["別のプロンプト", "重複テスト"]);
    }

    #[test]
    fn empty_display_is_excluded() {
        let input = r#"{"display":""}
{"display":"有効なプロンプト"}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result, vec!["有効なプロンプト"]);
    }

    #[test]
    fn missing_display_field_is_skipped() {
        let input = r#"{"other_field":"value"}
{"display":"有効"}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result, vec!["有効"]);
    }

    #[test]
    fn invalid_json_line_is_skipped() {
        let input = r#"not-json
{"display":"有効"}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result, vec!["有効"]);
    }

    #[test]
    fn whitespace_is_trimmed() {
        let input = r#"{"display":"  前後スペース  "}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result, vec!["前後スペース"]);
    }

    #[test]
    fn empty_input_returns_empty_vec() {
        let result = collect_prompts(lines(""));
        assert!(result.is_empty());
    }
}
