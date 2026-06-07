//! fzf を子プロセスとして起動し、ユーザーの選択結果を取得する層。
//!
//! 責務: fzf の起動・stdin への候補書き込み・stdout からの選択結果取得のみ。
//! 履歴パース・クリップボード・キーストロークは扱わない。

use std::io::Write;
use std::process::{Command, Stdio};

/// プレビュー用一時ファイルのスコープ終了時に自動削除する RAII ガード。
struct TmpFile(std::path::PathBuf);

impl Drop for TmpFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// `prompts` を fzf に渡してインタラクティブ選択させ、選択されたオリジナル全文を返す。
///
/// fzf には各プロンプトの先頭行のみを表示候補として渡し、選択後にインデックスで
/// オリジナル全文を逆引きする（複数行プロンプトが複数候補に分裂するバグを防ぐ）。
/// プレビューパネルには一時ファイル経由でプロンプト全文を表示する。
/// ユーザーが Esc 等でキャンセルした場合は None を返す。
/// fzf が見つからない場合も None を返す（エラーメッセージは stderr に出る）。
///
/// 複数行プロンプトは先頭行のみを fzf に表示し、選択後にオリジナル全文を返す。
/// インデックスをタブ区切りで付与して fzf 出力から逆引きするため、
/// 先頭行が重複するプロンプトが存在しても正しいエントリを返せる。
pub fn pick(prompts: &[String]) -> Option<String> {
    // プレビュー用一時ファイル: プロンプト全文を 1 行ずつ書き出す（改行は \x1f に置換）
    // ファイルは関数終了時に TmpFile::drop で自動削除される
    let tmp_path = std::env::temp_dir().join(format!("chp-{}.txt", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp_path).ok()?;
        for prompt in prompts {
            let _ = writeln!(f, "{}", escape_newlines(prompt));
        }
    }
    let _tmp_guard = TmpFile(tmp_path.clone());

    // {1} は fzf の 0-based インデックス、NR は 1-based のため +1 する
    // \037 は \x1f (Unit Separator) の octal 表記（BSD tr / GNU tr 共通）
    let preview_cmd = format!(
        "awk 'NR=={{1}}+1' '{}' | tr '\\037' '\\n'",
        tmp_path.to_string_lossy()
    );

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
            "--preview",
            &preview_cmd, // 一時ファイルからプロンプト全文を復元して表示
            "--preview-window",
            "down:10:wrap", // 複数行プロンプトが見やすいよう 10 行に拡大
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

/// プレビュー用一時ファイルへの書き出し形式に変換する。
///
/// 改行を \x1f (Unit Separator, octal \037) に置換することで、
/// 1 プロンプト = 1 行として書き出せる。
/// 復元は `tr '\037' '\n'` で行う（BSD / GNU tr 共通の octal エスケープ）。
fn escape_newlines(prompt: &str) -> String {
    prompt.replace('\n', "\x1f")
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

    #[test]
    fn escape_newlines_single_line() {
        assert_eq!(escape_newlines("hello world"), "hello world");
    }

    #[test]
    fn escape_newlines_multiline() {
        assert_eq!(escape_newlines("first\nsecond\nthird"), "first\x1fsecond\x1fthird");
    }

    #[test]
    fn escape_newlines_empty() {
        assert_eq!(escape_newlines(""), "");
    }

    #[test]
    fn escape_newlines_preserves_tabs() {
        // タブはそのまま保持（display_line が別途スペース変換する）
        assert_eq!(escape_newlines("a\tb"), "a\tb");
    }
}
