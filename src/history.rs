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
/// JSON パース失敗行はスキップし、ファイル全体の読み込みは続行する。
/// 一方で行読み込み中の IO エラー（NFS 断絶・ディスク EIO 等）は
/// 履歴欠落をサイレントに招くため、呼び出し元に伝播する。
pub fn load_prompts(history_path: &Path) -> std::io::Result<Vec<String>> {
    let file = File::open(history_path)?;
    load_prompts_from_reader(BufReader::new(file))
}

/// `BufRead` から JSONL を読み込み、表示用プロンプト一覧を返す。
/// IO エラー注入テストのために path 受領層から切り出している。
fn load_prompts_from_reader<R: BufRead>(reader: R) -> std::io::Result<Vec<String>> {
    let lines = reader.lines().collect::<std::io::Result<Vec<_>>>()?;
    Ok(collect_prompts(lines.into_iter()))
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
    use std::io::{self, Read};

    fn lines(raw: &str) -> impl Iterator<Item = String> + '_ {
        raw.lines().map(|l| l.to_string())
    }

    /// 1 回目の read で常に IO エラーを返す Reader。
    struct ErrorOnFirstRead;
    impl Read for ErrorOnFirstRead {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("simulated EIO"))
        }
    }

    /// 先に data を返してから次の read で IO エラーを返す Reader。
    /// ストリーム途中で IO エラーが起きるケースを再現する。
    struct DataThenError {
        data: Vec<u8>,
        pos: usize,
        errored: bool,
    }
    impl Read for DataThenError {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos < self.data.len() {
                let n = (self.data.len() - self.pos).min(buf.len());
                buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if !self.errored {
                self.errored = true;
                return Err(io::Error::other("simulated mid-stream EIO"));
            }
            Ok(0)
        }
    }

    #[test]
    fn io_error_at_start_is_propagated() {
        let result = load_prompts_from_reader(BufReader::new(ErrorOnFirstRead));
        let err = result.expect_err("先頭での IO エラーは Err として伝播すべき");
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn io_error_mid_stream_is_propagated_not_silenced() {
        // 過去実装は `lines().map(|l| l.unwrap_or_default())` で Err を空文字列に
        // 変換し、直後の空行スキップでサイレント破棄していた (Issue #33)。
        // ストリーム途中の IO エラーが Err として呼び出し元に届くことを保証する。
        let data = "{\"display\":\"前半行\"}\n".as_bytes().to_vec();
        let reader = BufReader::new(DataThenError {
            data,
            pos: 0,
            errored: false,
        });
        let result = load_prompts_from_reader(reader);
        assert!(result.is_err(), "ストリーム途中の IO エラーを伝播すべき");
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

    #[test]
    fn multiline_display_is_preserved_as_full_text() {
        // JSON の \n はパース後に実際の改行文字になる。
        // history 層はそのまま保持し、正規化は picker 層に委ねる。
        let input = r#"{"display":"line1\nline2"}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result, vec!["line1\nline2"]);
    }

    #[test]
    fn multiline_duplicate_dedup_uses_full_text() {
        let input = r#"{"display":"line1\nline2"}
{"display":"line1\nline2"}
{"display":"line1\nline3"}"#;
        let result = collect_prompts(lines(input));
        // collect_prompts は最新優先で返すため、ファイル末尾（line3）が先頭に来る
        assert_eq!(result, vec!["line1\nline3", "line1\nline2"]);
    }
}
