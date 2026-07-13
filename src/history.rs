//! 複数ソース（Claude Code / Codex CLI）共通のプロンプト表現とマージ層。
//!
//! 責務: `Prompt` / `Source` の定義、複数ソースの統合（フィルタ・時刻降順ソート・重複除去）のみ。
//! ソース別の JSONL パースは `claude.rs` / `codex.rs` が担う。UI（fzf）・クリップボード・
//! キーストロークは扱わない。

use chrono::{Local, TimeZone};
use std::collections::HashSet;
use std::io::{BufRead, BufReader};
use std::path::Path;

/// プロンプトの出どころ。fzf 表示の prefix・色分けに使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    Claude,
    Codex,
}

impl Source {
    /// fzf 表示 prefix に使うラベル（`[Claude]` / `[Codex]` の中身）。
    pub fn label(&self) -> &'static str {
        match self {
            Source::Claude => "Claude",
            Source::Codex => "Codex",
        }
    }

    /// fzf `--ansi` 用の前景色エスケープシーケンス。Claude = シアン、Codex = マゼンタ。
    pub fn ansi_color(&self) -> &'static str {
        match self {
            Source::Claude => "\x1b[36m",
            Source::Codex => "\x1b[35m",
        }
    }
}

/// フィルタリング・重複除去済みのプロンプト。
///
/// `display` は fzf リスト表示用テキスト。`pastedContents` がある行は
/// プレースホルダ形式（`[Pasted text #1 +48 lines]`）のまま保持する。
///
/// `full_text` はクリップボード・プレビュー用テキスト。
/// `~/.claude/paste-cache/{contentHash}.txt` が存在する場合はプレースホルダを
/// 実ペースト内容に展開済み。キャッシュが存在しない場合は `display` と同一。
///
/// `iso_timestamp` は表示用タイムスタンプ文字列（ローカル時刻）。
///
/// `timestamp_ms` はソート専用の数値表現（Unix ミリ秒）。複数ソースを時刻降順で
/// マージするために使う。`iso_timestamp` はレガシー `isoTimestamp` 由来の場合が
/// あり数値へ逆変換できないため、その場合は `None`（マージ時に最下位として扱う）。
#[derive(Debug)]
pub struct Prompt {
    pub(crate) source: Source,
    pub(crate) display: String,
    pub(crate) full_text: String,
    pub(crate) iso_timestamp: Option<String>,
    pub(crate) timestamp_ms: Option<i64>,
}

impl Prompt {
    /// プロンプトの出どころ（Claude / Codex）。
    pub fn source(&self) -> Source {
        self.source
    }

    /// fzf リスト表示用テキスト（`[Pasted text #N ...]` プレースホルダ形式を維持）。
    pub fn display(&self) -> &str {
        &self.display
    }

    /// クリップボード・プレビューパネル用テキスト（ペーストキャッシュを展開済み）。
    pub fn full_text(&self) -> &str {
        &self.full_text
    }

    /// 記録時刻のローカル ISO 文字列。`timestamp`/`isoTimestamp` なしの場合は `None`。
    pub fn timestamp(&self) -> Option<&str> {
        self.iso_timestamp.as_deref()
    }
}

/// Unix ミリ秒タイムスタンプをローカル時刻の ISO 8601 形式文字列に変換する。
///
/// UTC ではないため末尾に 'Z' を付けない。
/// 「いつ打ったか」を人間が読む用途なのでローカル時刻が適切。
/// Claude（ミリ秒）・Codex（秒→ミリ秒換算後）の双方から呼ばれる共通関数。
pub(crate) fn unix_ms_to_local_iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    Local
        .timestamp_opt(secs, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
        .unwrap_or_else(|| "1970-01-01T00:00:00".to_string())
}

/// `reader` から全行を `String` の `Vec` に読み込む。
///
/// Claude / Codex 両パーサが共有する「開く → 行を集める → パースに渡す」の
/// 前半部分を 1 箇所に集約する。行読み込み中の IO エラー（NFS 断絶・ディスク
/// EIO 等）は履歴欠落をサイレントに招くため、呼び出し元に伝播する。
pub(crate) fn read_lines<R: BufRead>(reader: R) -> std::io::Result<Vec<String>> {
    reader.lines().collect()
}

/// `path` のファイルを開き `read_lines` で全行を読み込む。
pub(crate) fn read_lines_from_path(path: &Path) -> std::io::Result<Vec<String>> {
    let file = std::fs::File::open(path)?;
    read_lines(BufReader::new(file))
}

/// fzf 表示候補として採用すべきプロンプトか判定する。
///
/// 除外条件:
/// - 単独スラッシュコマンド形式（`/help` `/code-review` 等、Codex CLI の
///   `/model` `/diff` 等も同形式）: 各ツール内蔵の `/` メニューで補えるため
///   fzf に出す価値が低い。引数や記号を伴うもの（`/loop 5m /foo` `/foo:bar`）
///   は再利用価値が高いため採用する
fn is_eligible(display: &str) -> bool {
    !is_bare_slash_command(display)
}

/// 単独スラッシュコマンド判定: `/` + 英数/ハイフン/アンダースコアのみ。
///
/// Claude Code・Codex CLI いずれのスラッシュコマンド名も ASCII 英数 + `-` + `_`
/// で構成される。この形式に完全一致するものは内蔵 `/` メニューで補えるため fzf
/// 候補から除外する。判定軸を「空白の有無」ではなく「単独 slash 形式か」に置くこ
/// とで、Unicode 空白混入（`/foo　bar`）や記号区切り（`/foo:bar`）など
/// 「文字列として価値のある」ケースも自然に採用側に倒れる。
fn is_bare_slash_command(s: &str) -> bool {
    let Some(name) = s.strip_prefix('/') else {
        return false;
    };
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// 複数ソースのプロンプトを統合する: フィルタ → 時刻降順ソート → 重複除去の 3 段。
///
/// - フィルタ: `is_eligible`（単独スラッシュコマンド除外）
/// - ソート: `timestamp_ms` 降順（新しい順）。`None` は最下位。同値はソース間の
///   入力順を保持（安定ソート）
/// - 重複除去: `(source, display)` をキーに最初に現れたもの（= 最新）を残す。
///   同一テキストでもソースが異なれば両方残す（どちらのツールで打ったか区別する）
pub fn merge_sort_dedup(all: Vec<Prompt>) -> Vec<Prompt> {
    let mut eligible: Vec<Prompt> = all
        .into_iter()
        .filter(|p| is_eligible(&p.display))
        .collect();
    eligible.sort_by_key(|p| std::cmp::Reverse(p.timestamp_ms));

    let mut seen: HashSet<(Source, String)> = HashSet::new();
    eligible
        .into_iter()
        .filter(|p| seen.insert((p.source, p.display.clone())))
        .collect()
}

/// テスト専用の `Prompt` ビルダー。`claude.rs` / `codex.rs` / `main.rs` / `picker.rs`
/// のテストから共有し、struct literal の重複と `source`/`timestamp_ms` 追加時の
/// 書き換え漏れを防ぐ。
#[cfg(test)]
pub(crate) mod test_support {
    use super::{unix_ms_to_local_iso, Prompt, Source};

    /// `display == full_text` の最小構成で `Prompt` を組み立てる。
    pub(crate) fn make_prompt(source: Source, display: &str, timestamp_ms: Option<i64>) -> Prompt {
        make_prompt_with_full_text(source, display, display, timestamp_ms)
    }

    /// `display` と `full_text` を別々に指定できる版（ペースト展開・複数行プレビュー確認用）。
    pub(crate) fn make_prompt_with_full_text(
        source: Source,
        display: &str,
        full_text: &str,
        timestamp_ms: Option<i64>,
    ) -> Prompt {
        Prompt {
            source,
            display: display.to_string(),
            full_text: full_text.to_string(),
            iso_timestamp: timestamp_ms.map(unix_ms_to_local_iso),
            timestamp_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::make_prompt as prompt;
    use super::*;
    use std::io::{self, Read};

    /// 1 回目の read で常に IO エラーを返す Reader（`read_lines` の伝播確認用）。
    struct ErrorOnFirstRead;
    impl Read for ErrorOnFirstRead {
        fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::other("simulated EIO"))
        }
    }

    #[test]
    fn read_lines_propagates_io_error() {
        // claude.rs / codex.rs 共有の read_lines がストリームエラーをサイレントに
        // 握りつぶさないことを保証する（Issue #33 の再発防止と同じ意図）。
        let result = read_lines(BufReader::new(ErrorOnFirstRead));
        let err = result.expect_err("IO エラーは Err として伝播すべき");
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn read_lines_from_path_propagates_not_found() {
        let result = read_lines_from_path(Path::new("/definitely/does/not/exist/history.jsonl"));
        let err = result.expect_err("存在しないパスは Err を返すべき");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    fn displays(prompts: &[Prompt]) -> Vec<(Source, String)> {
        prompts
            .iter()
            .map(|p| (p.source, p.display.clone()))
            .collect()
    }

    #[test]
    fn bare_slash_command_is_excluded() {
        let input = vec![
            prompt(Source::Claude, "/help", Some(100)),
            prompt(Source::Claude, "通常のプロンプト", Some(200)),
        ];
        let result = merge_sort_dedup(input);
        assert_eq!(
            displays(&result),
            vec![(Source::Claude, "通常のプロンプト".to_string())]
        );
    }

    #[test]
    fn slash_command_with_args_is_included() {
        let input = vec![
            prompt(Source::Codex, "/model gpt-5", Some(100)),
            prompt(Source::Codex, "/diff", Some(200)),
        ];
        let result = merge_sort_dedup(input);
        assert_eq!(
            displays(&result),
            vec![(Source::Codex, "/model gpt-5".to_string())]
        );
    }

    #[test]
    fn lone_slash_is_not_treated_as_bare_command() {
        let input = vec![prompt(Source::Claude, "/", Some(100))];
        let result = merge_sort_dedup(input);
        assert_eq!(displays(&result), vec![(Source::Claude, "/".to_string())]);
    }

    #[test]
    fn sorted_by_timestamp_descending_across_sources() {
        let input = vec![
            prompt(Source::Claude, "古い Claude", Some(100)),
            prompt(Source::Codex, "新しい Codex", Some(300)),
            prompt(Source::Claude, "中間 Claude", Some(200)),
        ];
        let result = merge_sort_dedup(input);
        assert_eq!(
            displays(&result),
            vec![
                (Source::Codex, "新しい Codex".to_string()),
                (Source::Claude, "中間 Claude".to_string()),
                (Source::Claude, "古い Claude".to_string()),
            ]
        );
    }

    #[test]
    fn missing_timestamp_sorts_last() {
        let input = vec![
            prompt(Source::Claude, "タイムスタンプなし", None),
            prompt(Source::Claude, "タイムスタンプあり", Some(100)),
        ];
        let result = merge_sort_dedup(input);
        assert_eq!(
            displays(&result),
            vec![
                (Source::Claude, "タイムスタンプあり".to_string()),
                (Source::Claude, "タイムスタンプなし".to_string()),
            ]
        );
    }

    #[test]
    fn same_display_different_source_both_kept() {
        // 同一テキストでも source が違えば両方残す（どちらのツールで打ったか区別する）。
        let input = vec![
            prompt(Source::Claude, "共通コマンド", Some(100)),
            prompt(Source::Codex, "共通コマンド", Some(200)),
        ];
        let result = merge_sort_dedup(input);
        assert_eq!(
            displays(&result),
            vec![
                (Source::Codex, "共通コマンド".to_string()),
                (Source::Claude, "共通コマンド".to_string()),
            ]
        );
    }

    #[test]
    fn same_source_same_display_dedup_keeps_latest() {
        let input = vec![
            prompt(Source::Claude, "重複", Some(100)),
            prompt(Source::Claude, "重複", Some(300)),
            prompt(Source::Claude, "重複", Some(200)),
        ];
        let result = merge_sort_dedup(input);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].timestamp_ms, Some(300));
    }

    #[test]
    fn empty_input_returns_empty_vec() {
        assert!(merge_sort_dedup(Vec::new()).is_empty());
    }

    #[test]
    fn unix_ms_to_local_iso_converts_known_timestamp() {
        // 1780928372000 ms ≈ 2026-06-08 JST
        let iso = unix_ms_to_local_iso(1780928372000);
        assert!(
            iso.contains("2026"),
            "2026 年のタイムスタンプになるべき: {iso}"
        );
    }
}
