//! fzf を子プロセスとして起動し、ユーザーの選択結果を取得する層。
//!
//! 責務: fzf の起動・stdin への候補書き込み・stdout からの選択結果取得のみ。
//! 履歴パース・クリップボード・キーストロークは扱わない。

use std::io::Write;
use std::process::{Command, Stdio};

/// `prompts` を fzf に渡してインタラクティブ選択させ、選択された文字列を返す。
///
/// ユーザーが Esc 等でキャンセルした場合は None を返す。
/// fzf が見つからない場合も None を返す（エラーメッセージは stderr に出る）。
///
/// 複数行プロンプトは先頭行のみを fzf に表示し、選択後にオリジナル全文を返す。
/// インデックスをタブ区切りで付与して fzf 出力から逆引きするため、
/// 先頭行が重複するプロンプトが存在しても正しいエントリを返せる。
pub fn pick(prompts: &[String]) -> Option<String> {
    let mut child = Command::new("fzf")
        .args([
            "--height",
            "100%",      // ターミナル全体を使う
            "--reverse", // 候補を上から下に表示（プロンプトが上）
            "--prompt",
            "Claude History > ",
            "--delimiter",
            "\t", // フィールド区切りをタブに設定
            "--with-nth",
            "2..", // インデックス列（1列目）を表示から除外
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| {
            eprintln!("fzf の起動に失敗しました: {e}");
            eprintln!("brew install fzf で fzf をインストールしてください");
        })
        .ok()?;

    // stdin に "{index}\t{display_line}" を 1 行ずつ書き込む
    if let Some(mut stdin) = child.stdin.take() {
        for (i, prompt) in prompts.iter().enumerate() {
            let _ = writeln!(stdin, "{}\t{}", i, display_line(prompt));
        }
        // stdin を drop することで fzf 側の EOF が発生し、候補リストが確定する
    }

    let fzf_output = child.wait_with_output().ok()?;

    if fzf_output.status.success() {
        let selected = String::from_utf8(fzf_output.stdout).ok()?;
        let trimmed = selected.trim();
        if trimmed.is_empty() {
            return None;
        }
        // fzf は --with-nth でも行全体を返すため "{index}\t{display}" をパース
        let idx: usize = trimmed.split('\t').next()?.parse().ok()?;
        prompts.get(idx).cloned()
    } else {
        // exit code 130 = Ctrl-C / Esc によるキャンセル
        None
    }
}

/// fzf 表示用に 1 行へ正規化する。
///
/// 複数行プロンプトは先頭行のみを返す。タブ文字はフィールド区切りと
/// 衝突するためスペースに置換する。
fn display_line(prompt: &str) -> String {
    prompt.lines().next().unwrap_or("").replace('\t', " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_line_single_line() {
        assert_eq!(display_line("hello world"), "hello world");
    }

    #[test]
    fn display_line_multiline_returns_first() {
        assert_eq!(display_line("first\nsecond\nthird"), "first");
    }

    #[test]
    fn display_line_replaces_tabs() {
        assert_eq!(display_line("tab\there"), "tab here");
    }

    #[test]
    fn display_line_empty_string() {
        assert_eq!(display_line(""), "");
    }

    #[test]
    fn display_line_only_newline() {
        assert_eq!(display_line("\n"), "");
    }
}
