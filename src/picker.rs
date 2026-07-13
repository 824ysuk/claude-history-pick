//! fzf を子プロセスとして起動し、ユーザーの選択結果を取得する層。
//!
//! 責務: fzf の起動・stdin への候補書き込み・stdout からの選択結果取得のみ。
//! 履歴パース・クリップボード・キーストロークは扱わない。

use crate::history::Prompt;
use std::io::Write;
use std::process::{Command, Stdio};

use tempfile::NamedTempFile;

/// ANSI エスケープをリセットするシーケンス。`Source::ansi_color()` で色付けした
/// prefix の直後に必ず付与する。
const ANSI_RESET: &str = "\x1b[0m";

/// `prompts` を fzf に渡してインタラクティブ選択させ、選択されたオリジナル全文を返す。
///
/// fzf には各プロンプトの先頭行のみを表示候補として渡し、選択後にインデックスで
/// オリジナル全文を逆引きする（複数行プロンプトが複数候補に分裂するバグを防ぐ）。
/// プレビューパネルには一時ファイル経由でプロンプト全文と記録時刻を表示する。
/// ユーザーが Esc 等でキャンセルした場合は None を返す。
/// fzf が見つからない場合も None を返す（エラーメッセージは stderr に出る）。
///
/// 複数行プロンプトは先頭行のみを fzf に表示し、選択後にオリジナル全文を返す。
/// インデックスをタブ区切りで付与して fzf 出力から逆引きするため、
/// 先頭行が重複するプロンプトが存在しても正しいエントリを返せる。
pub fn pick(prompts: &[Prompt]) -> Option<String> {
    // プレビュー用一時ファイル: tempfile はランダム名 + O_CREAT|O_EXCL で生成するため
    // 予測可能パスによる symlink attack を防げる。NamedTempFile は Drop で自動削除。
    let mut tmp = NamedTempFile::new().ok()?;
    for prompt in prompts {
        let ts = prompt.timestamp().unwrap_or("");
        let _ = writeln!(
            tmp,
            "{}\t{}\t{}",
            escape_newlines(prompt.full_text()),
            ts,
            prompt.source().label()
        );
    }
    let _ = tmp.flush();
    let tmp_path = tmp.path().to_path_buf();

    let preview_cmd = build_preview_cmd(&tmp_path.to_string_lossy());

    let mut child = Command::new("fzf")
        .args(build_fzf_args(&preview_cmd))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| {
            eprintln!("fzf の起動に失敗しました: {e}");
            eprintln!("brew install fzf で fzf をインストールしてください");
        })
        .ok()?;

    // stdin に "{index}\t{source_prefix}{display_line}" を 1 行ずつ書き込む
    if let Some(mut stdin) = child.stdin.take() {
        for (i, prompt) in prompts.iter().enumerate() {
            let _ = writeln!(stdin, "{}\t{}", i, display_line_with_source(prompt));
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
        prompts.get(idx).map(|p| p.full_text().to_string())
    } else {
        // exit code 130 = Ctrl-C / Esc によるキャンセル
        None
    }
}

/// fzf 起動引数を組み立てる（純粋関数）。`--preview` のみ呼び出し時点の一時ファイル
/// パスに依存する動的値で、他は固定値。テストで `--ansi` 等の必須オプション漏れを
/// 機械的に検出できるよう、`Command::args` へのインライン列挙から切り出している。
fn build_fzf_args(preview_cmd: &str) -> Vec<&str> {
    vec![
        "--height",
        "100%",      // ターミナル全体を使う
        "--reverse", // 候補を上から下に表示（プロンプトが上）
        "--no-sort", // 入力順（最新→最古）を保持。デフォルトのスコアソートは文字列が
        // 短いほど高スコアになり、長いプロンプトが古い短いバリアントより
        // 下に押し出される。ヒストリピッカーではマッチ品質より使用時刻の
        // 新しさを優先するため無効化する。
        "--ansi", // source prefix の色（[Claude]/[Codex]）を解釈しつつ、検索対象は
        // プレーンテキストのまま扱う（fzf は ANSI エスケープを表示専用として除去する）。
        "--prompt",
        "History > ",
        "--delimiter",
        "\t", // フィールド区切りをタブに設定
        "--with-nth",
        "2..", // インデックス列（1列目）を表示から除外
        "--preview",
        preview_cmd, // 一時ファイルからプロンプト全文と時刻を復元して表示
        "--preview-window",
        "down:10:wrap", // 複数行プロンプトが見やすいよう 10 行に拡大
    ]
}

/// fzf 表示用に 1 行へ正規化する。
///
/// 複数行プロンプトは先頭行のみを返す。タブ文字はフィールド区切りと
/// 衝突するためスペースに置換する。
fn display_line(prompt: &str) -> String {
    prompt.lines().next().unwrap_or("").replace('\t', " ")
}

/// `display_line` の結果に色付き `[Source]` prefix を付与する。
///
/// 色・ラベルは `Source::ansi_color` / `Source::label`（history.rs）から取る。
/// ソースを追加するたびに picker 側の分岐を増やさずに済むよう、色分けの
/// 責務は `Source` 側に閉じ込めている。
fn display_line_with_source(prompt: &Prompt) -> String {
    format!(
        "{color}[{label}]{ANSI_RESET} {line}",
        color = prompt.source().ansi_color(),
        label = prompt.source().label(),
        line = display_line(prompt.display())
    )
}

/// プレビュー用一時ファイルへの書き出し形式に変換する。
///
/// 改行を \x1f (Unit Separator, octal \037)、タブを \x1e (Record Separator, octal \036)
/// に置換することで、1 プロンプト = 1 行として書き出せる。
/// 復元は awk 内の gsub で行う。タブを別途エスケープするのは、一時ファイルの
/// フィールド区切りがタブであるため full_text 内の literal タブが awk の $1/$2 分割を崩すため。
fn escape_newlines(prompt: &str) -> String {
    prompt.replace('\n', "\x1f").replace('\t', "\x1e")
}

/// fzf に渡す preview コマンド文字列を構築する（純粋関数）。
///
/// 一時ファイルの各行形式: `{full_text_escaped}\t{iso_timestamp_or_empty}\t{source_label}`
/// - `{1}` は fzf の 0-based インデックスプレースホルダ、awk の `NR` は 1-based のため +1 で補正
/// - ヘッダとして `[Claude]` / `[Codex]`（`$3`）を常に表示し、タイムスタンプ（`$2`）が
///   存在する場合は続けて `[YYYY-MM-DDTHH:MM:SS]` を表示する
/// - `\037` (Unit Separator) を `\n` に、`\036` (Record Separator) を `\t` に戻してプロンプト全文を表示
/// - `tmp_path` は POSIX シェルエスケープで囲みインジェクションを防ぐ
fn build_preview_cmd(tmp_path: &str) -> String {
    format!(
        "awk -F'\\t' 'NR=={{1}}+1 {{ header = \"[\" $3 \"]\"; if ($2 != \"\") header = header \" [\" $2 \"]\"; printf \"%s\\n\\n\", header; gsub(/\\037/, \"\\n\", $1); gsub(/\\036/, \"\\t\", $1); print $1 }}' {}",
        posix_shell_quote(tmp_path)
    )
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
    use crate::history::test_support::{make_prompt, make_prompt_with_full_text};
    use crate::history::Source;

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
    fn escape_newlines_escapes_tabs() {
        // タブはフィールド区切り衝突防止のため \x1e (Record Separator) に置換する。
        // awk で gsub(/\036/, "\t") により復元される。
        assert_eq!(escape_newlines("a\tb"), "a\x1eb");
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

    #[test]
    fn build_preview_cmd_includes_index_plus_one() {
        let cmd = build_preview_cmd("/tmp/chp-abc.txt");
        assert!(
            cmd.contains("NR=={1}+1"),
            "0-based → 1-based 補正が欠落: {cmd}"
        );
    }

    #[test]
    fn build_preview_cmd_quotes_tmp_path() {
        let cmd = build_preview_cmd("/tmp/chp-abc.txt");
        assert!(
            cmd.contains("'/tmp/chp-abc.txt'"),
            "tmp_path がシェルクォートされていない: {cmd}"
        );
    }

    #[test]
    fn build_preview_cmd_restores_newlines_via_gsub() {
        let cmd = build_preview_cmd("/tmp/chp-abc.txt");
        assert!(
            cmd.contains("gsub(/\\037/, \"\\n\", $1)"),
            "gsub による改行復元が欠落: {cmd}"
        );
    }

    #[test]
    fn build_preview_cmd_restores_tabs_via_gsub() {
        let cmd = build_preview_cmd("/tmp/chp-abc.txt");
        assert!(
            cmd.contains("gsub(/\\036/, \"\\t\", $1)"),
            "gsub によるタブ復元が欠落: {cmd}"
        );
    }

    #[test]
    fn build_preview_cmd_escapes_single_quote_in_path() {
        let cmd = build_preview_cmd("/tmp/a';rm -rf /;'b");
        assert!(
            cmd.contains(r#"'/tmp/a'\'';rm -rf /;'\''b'"#),
            "シングルクォートが安全にエスケープされていない: {cmd}"
        );
    }

    #[test]
    fn build_preview_cmd_uses_tab_as_field_separator() {
        let cmd = build_preview_cmd("/tmp/chp-abc.txt");
        assert!(
            cmd.contains("-F'\\t'"),
            "awk フィールド区切り -F'\\t' が欠落: {cmd}"
        );
    }

    #[test]
    fn escape_newlines_roundtrip_via_tr() {
        let original = "line1\nline2\nline3\n複数行\nテスト";
        let escaped = escape_newlines(original);
        assert!(
            !escaped.contains('\n'),
            "escape 後に LF が残っている: {escaped:?}"
        );

        let mut child = Command::new("tr")
            .args(["\\037", "\\n"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("tr の起動に失敗");
        {
            let mut stdin = child.stdin.take().expect("stdin");
            stdin
                .write_all(escaped.as_bytes())
                .expect("stdin write 失敗");
        }
        let out = child.wait_with_output().expect("tr wait 失敗");
        let restored = String::from_utf8(out.stdout).expect("UTF-8 復元失敗");
        assert_eq!(
            restored, original,
            "escape_newlines → gsub のラウンドトリップが一致しない"
        );
    }

    // --- preview temp file はescape_newlines(full_text) を書き出すことを検証 ---
    // pick() は fzf を起動するため直接テスト不可だが、
    // escape_newlines が full_text に適用されることを間接的に保証するため
    // 「full_text に改行が含まれていても正しくエスケープされる」ことを確認する。

    #[test]
    fn full_text_with_newlines_is_escaped_to_unit_separator() {
        // full_text の改行 (LF) が \x1f に置換されることで一時ファイル内で 1 行に収まる。
        let full = "line1\nline2\nline3";
        let escaped = escape_newlines(full);
        assert!(!escaped.contains('\n'), "LF が残っている: {escaped:?}");
        assert_eq!(escaped.matches('\x1f').count(), 2);
    }

    #[test]
    fn full_text_without_newlines_is_unchanged_after_escape() {
        let full = "[Pasted text #1 +3 lines]";
        assert_eq!(escape_newlines(full), full);
    }

    #[test]
    fn display_line_of_placeholder_is_single_line() {
        // display に改行がない場合は display_line もそのまま。
        let p = make_prompt(Source::Claude, "[Pasted text #1 +3 lines]", None);
        assert_eq!(display_line(&p.display), "[Pasted text #1 +3 lines]");
    }

    #[test]
    fn display_line_of_full_text_returns_first_line() {
        // full_text が複数行でも display_line は先頭行のみ返す。
        let p = make_prompt_with_full_text(
            Source::Claude,
            "[Pasted text #1 +3 lines]",
            "line1\nline2\nline3",
            None,
        );
        assert_eq!(display_line(&p.full_text), "line1");
    }

    // ─── source prefix ──────────────────────────────────────────────────────

    #[test]
    fn display_line_with_source_prefixes_claude() {
        let p = make_prompt(Source::Claude, "ビルドして", None);
        let line = display_line_with_source(&p);
        assert!(
            line.starts_with("\x1b[36m[Claude]\x1b[0m "),
            "実際: {line:?}"
        );
        assert!(line.ends_with("ビルドして"));
    }

    #[test]
    fn display_line_with_source_prefixes_codex() {
        let p = make_prompt(Source::Codex, "テストして", None);
        let line = display_line_with_source(&p);
        assert!(
            line.starts_with("\x1b[35m[Codex]\x1b[0m "),
            "実際: {line:?}"
        );
        assert!(line.ends_with("テストして"));
    }

    #[test]
    fn display_line_with_source_uses_first_line_only() {
        // display_line の「先頭行のみ」規約を display_line_with_source も継承する。
        let p = make_prompt_with_full_text(Source::Codex, "1st\n2nd", "1st\n2nd", None);
        let line = display_line_with_source(&p);
        assert!(!line.contains('\n'), "改行が残っている: {line:?}");
        assert!(line.ends_with("1st"));
    }

    // ─── preview ヘッダ（source + timestamp） ──────────────────────────────

    #[test]
    fn build_preview_cmd_header_always_includes_source_column() {
        // $3（source label）は常にヘッダへ含む。
        let cmd = build_preview_cmd("/tmp/chp-abc.txt");
        assert!(
            cmd.contains("header = \"[\" $3 \"]\""),
            "source label をヘッダに含める節が欠落: {cmd}"
        );
    }

    #[test]
    fn build_preview_cmd_header_appends_timestamp_when_present() {
        let cmd = build_preview_cmd("/tmp/chp-abc.txt");
        assert!(
            cmd.contains("if ($2 != \"\") header = header \" [\" $2 \"]\""),
            "タイムスタンプ追記節が欠落: {cmd}"
        );
    }

    // ─── fzf 起動引数 ───────────────────────────────────────────────────────

    #[test]
    fn build_fzf_args_includes_ansi() {
        // --ansi がないと source prefix の色エスケープが生の文字列として表示されてしまう。
        let args = build_fzf_args("dummy-preview-cmd");
        assert!(args.contains(&"--ansi"), "実際: {args:?}");
    }

    #[test]
    fn build_fzf_args_includes_no_sort() {
        let args = build_fzf_args("dummy-preview-cmd");
        assert!(args.contains(&"--no-sort"), "実際: {args:?}");
    }

    #[test]
    fn build_fzf_args_uses_tab_delimiter_matching_stdin_format() {
        // stdin は "{index}\t{...}" 形式で書き込む（pick 関数）ため、fzf 側の
        // --delimiter もタブでなければならない。ここがずれるとインデックス列の
        // 逆引きが壊れる（fzf は最初のフィールドを表示から除外できなくなる）。
        let args = build_fzf_args("dummy-preview-cmd");
        let delim_idx = args
            .iter()
            .position(|a| *a == "--delimiter")
            .expect("--delimiter が見つからない");
        assert_eq!(args[delim_idx + 1], "\t");
    }

    #[test]
    fn build_fzf_args_embeds_preview_cmd_verbatim() {
        let args = build_fzf_args("my-unique-preview-command");
        assert!(args.contains(&"my-unique-preview-command"));
    }
}
