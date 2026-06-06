//! Claude Code の ~/.claude/history.jsonl からプロンプト履歴を読み込む層。
//!
//! 責務: JSON パース・フィルタリング・重複除去のみ。
//! UI（fzf）・クリップボード・キーストロークは扱わない。

use serde::Deserialize;
use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;

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
pub fn load_prompts(history_path: &PathBuf) -> std::io::Result<Vec<String>> {
    let file = File::open(history_path)?;
    let reader = BufReader::new(file);

    let mut prompts = Vec::new();
    let mut seen = HashSet::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let entry: HistoryEntry = match serde_json::from_str(&line) {
            Ok(e) => e,
            Err(_) => continue, // 不正な JSON 行は無視
        };

        if let Some(display) = entry.display {
            let display = display.trim().to_string();
            if display.is_empty() || display.starts_with('/') {
                continue; // 空行・スラッシュコマンドを除外
            }
            if seen.insert(display.clone()) {
                prompts.push(display); // 初出のみ追加
            }
        }
    }

    Ok(prompts)
}
