//! fzf を子プロセスとして起動し、ユーザーの選択結果を取得する層。
//!
//! 責務: fzf の起動・stdin への候補書き込み・stdout からの選択結果取得のみ。
//! 履歴パース・クリップボード・キーストロークは扱わない。

use std::io::Write;
use std::process::{Command, Stdio};

use tempfile::NamedTempFile;

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
    // プレビュー用一時ファイル: tempfile はランダム名 + O_CREAT|O_EXCL で生成するため
    // 予測可能パスによる symlink attack を防げる。NamedTempFile は Drop で自動削除。
    let mut tmp = NamedTempFile::new().ok()?;
    for prompt in prompts {
        let _ = writeln!(tmp, "{}", escape_newlines(prompt));
    }
    let _ = tmp.flush();
    let tmp_path = tmp.path().to_path_buf();

    // {1} は fzf の 0-based インデックス、NR は 1-based のため +1 する
    // \037 は \x1f (Unit Separator) の octal 表記（BSD tr / GNU tr 共通）
    // パスは POSIX シェルエスケープで囲む（TMPDIR が攻撃者制御下にあっても安全）
    let preview_cmd = format!(
        "awk 'NR=={{1}}+1' {} | tr '\\037' '\\n'",
        posix_shell_quote(&tmp_path.to_string_lossy())
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

/// POSIX シェルのシングルクォート文字列としてエスケープする。
///
/// シングルクォート内では `'` 以外すべて literal として扱われるため、
/// `'` を `'\''`（閉じ → エスケープした `'` → 再オープン）に置換すれば任意文字列を安全に表せる。
/// preview_cmd は `sh -c` 経由で実行されるため、TMPDIR や tempfile パスに将来
/// シングルクォート等が混入してもコマンドインジェクションを防げる。
fn posix_shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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
        assert_eq!(
            escape_newlines("first\nsecond\nthird"),
            "first\x1fsecond\x1fthird"
        );
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

    #[test]
    fn posix_shell_quote_plain_path() {
        assert_eq!(posix_shell_quote("/tmp/chp-abc.txt"), "'/tmp/chp-abc.txt'");
    }

    #[test]
    fn posix_shell_quote_empty() {
        assert_eq!(posix_shell_quote(""), "''");
    }

    #[test]
    fn posix_shell_quote_escapes_single_quote() {
        // 攻撃者制御 TMPDIR でシングルクォート + コマンドが混入したケース。
        // 結果は閉じ→エスケープ済 `'`→再オープン形式で literal として扱われる。
        assert_eq!(
            posix_shell_quote("/tmp/a';rm -rf /;'b"),
            r#"'/tmp/a'\'';rm -rf /;'\''b'"#
        );
    }

    #[test]
    fn posix_shell_quote_handles_spaces_and_specials() {
        assert_eq!(
            posix_shell_quote("/tmp/path with $space/`cmd`"),
            "'/tmp/path with $space/`cmd`'"
        );
    }
}
