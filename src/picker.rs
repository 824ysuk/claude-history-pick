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
pub fn pick(prompts: &[String]) -> Option<String> {
    let mut child = Command::new("fzf")
        .args([
            "--height",
            "100%",      // ターミナル全体を使う
            "--reverse", // 候補を上から下に表示（プロンプトが上）
            "--prompt",
            "Claude History > ",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| {
            eprintln!("fzf の起動に失敗しました: {e}");
            eprintln!("brew install fzf で fzf をインストールしてください");
        })
        .ok()?;

    // stdin に候補を 1 行ずつ書き込む（take で所有権を得て drop で close）
    if let Some(mut stdin) = child.stdin.take() {
        for prompt in prompts {
            let _ = writeln!(stdin, "{}", prompt);
        }
        // stdin を drop することで fzf 側の EOF が発生し、候補リストが確定する
    }

    let fzf_output = child.wait_with_output().ok()?;

    if fzf_output.status.success() {
        let selected = String::from_utf8(fzf_output.stdout).ok()?;
        let trimmed = selected.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    } else {
        // exit code 130 = Ctrl-C / Esc によるキャンセル
        None
    }
}
