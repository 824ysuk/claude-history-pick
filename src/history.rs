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
/// - 引数なしスラッシュコマンド（`/help` 等、空白を含まない '/' 始まり）を除外。
///   引数付き（`/loop 5m /foo` 等）は再利用価値が高いため採用する
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

/// JSONL 1 行から表示用テキスト（`display` フィールド）を取り出す。
///
/// 空行・JSON パース失敗・`display` 欠落・空文字列はすべて `None` を返す。
/// 上流で `filter_map` に渡すことを想定。
fn parse_display(line: &str) -> Option<String> {
    if line.trim().is_empty() {
        return None;
    }
    let entry: HistoryEntry = serde_json::from_str(line).ok()?;
    let display = entry.display?.trim().to_string();
    if display.is_empty() {
        None
    } else {
        Some(display)
    }
}

/// fzf 表示候補として採用すべきプロンプトか判定する。
///
/// 除外条件:
/// - 引数なしスラッシュコマンド（`/help` `/clear` 等）: Claude Code 内で `/`
///   キーからメニュー選択できるため fzf に出す価値が低い。
///   引数付き（`/loop 5m /foo` 等）は手入力が長く再利用価値が高いため採用する。
fn is_eligible(display: &str) -> bool {
    !is_bare_slash_command(display)
}

/// 引数なしスラッシュコマンド判定: '/' 始まり かつ ASCII 空白を含まない。
fn is_bare_slash_command(s: &str) -> bool {
    s.starts_with('/') && !s.chars().any(|c| c.is_ascii_whitespace())
}

/// 最新出現を優先して重複除去する（fzf の体感に合わせ末尾優先）。
///
/// 入力は時系列（古い→新しい）を想定。同一文字列は最後の出現位置だけ残し、
/// 新しい順（新→古）に並び替えて返す。
fn dedup_keep_last(prompts: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut result = Vec::with_capacity(prompts.len());
    for p in prompts.into_iter().rev() {
        if seen.insert(p.clone()) {
            result.push(p);
        }
    }
    result
}

/// JSONL 行イテレータからプロンプトを収集する（テスト可能な純粋処理層）。
///
/// 構成: parse → filter → dedup の 3 段。各段はファイル内の private 関数として
/// 単独で意味を持つ。ファイル I/O を伴わないため失敗しない（返り値を Result に
/// しないことでその事実を型で表現する）。
pub fn collect_prompts(lines: impl Iterator<Item = String>) -> Vec<String> {
    let eligible: Vec<String> = lines
        .filter_map(|l| parse_display(&l))
        .filter(|d| is_eligible(d))
        .collect();
    dedup_keep_last(eligible)
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
    fn bare_slash_command_is_excluded() {
        let input = r#"{"display":"/help"}
{"display":"通常のプロンプト"}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result, vec!["通常のプロンプト"]);
    }

    #[test]
    fn slash_command_with_args_is_included() {
        // 引数付き（`/loop 5m /foo` 等）は手入力が長く再利用価値が高いため採用する。
        let input = r#"{"display":"/loop 5m /foo"}
{"display":"/code-review --comment"}
{"display":"/help"}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result, vec!["/code-review --comment", "/loop 5m /foo"]);
    }

    #[test]
    fn slash_command_with_tab_is_included() {
        // ASCII 空白には tab も含まれる。tab 区切りも引数付きとして採用する。
        let input = "{\"display\":\"/foo\\tbar\"}";
        let result = collect_prompts(lines(input));
        assert_eq!(result, vec!["/foo\tbar"]);
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
