//! Claude Code の ~/.claude/history.jsonl からプロンプト履歴を読み込む層。
//!
//! 責務: JSON パース・フィルタリング・重複除去・ペーストキャッシュ展開のみ。
//! UI（fzf）・クリップボード・キーストロークは扱わない。

use serde::Deserialize;
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// history.jsonl の pastedContents エントリ 1 件。
#[derive(Deserialize)]
struct PasteRef {
    id: u32,
    #[serde(rename = "contentHash")]
    content_hash: String,
}

/// history.jsonl の 1 行に対応する構造体。
///
/// `isoTimestamp` はレガシーフィールド（現行 Claude Code は書かない）。
/// `timestamp` が現行フォーマット（Unix ミリ秒）。
#[derive(Deserialize)]
struct HistoryEntry {
    display: Option<String>,
    #[serde(rename = "isoTimestamp")]
    iso_timestamp: Option<String>,
    timestamp: Option<u64>,
    #[serde(rename = "pastedContents")]
    pasted_contents: Option<HashMap<String, PasteRef>>,
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
/// `isoTimestamp` フィールドを持つ旧形式はそのまま、`timestamp`（数値 ms）を
/// 持つ現行形式は ISO 8601 相当のローカル時刻文字列に変換する。
#[derive(Debug)]
pub struct Prompt {
    pub display: String,
    pub full_text: String,
    pub iso_timestamp: Option<String>,
}

/// `history_path` の JSONL を読み込み、表示用プロンプト一覧を返す。
///
/// paste-cache ディレクトリは `history_path` の親ディレクトリ内の
/// `paste-cache/` サブディレクトリを使う（`~/.claude/paste-cache/`）。
///
/// フィルタ条件:
/// - `display` フィールドが存在し、空でない行のみ採用
/// - 単独スラッシュコマンド形式（`/help` `/code-review` 等、`/` + 英数/ハイフン/
///   アンダースコアのみ）を除外。引数や記号を伴うもの（`/loop 5m /foo` `/foo:bar`）
///   は再利用価値が高いため採用する
/// - 重複エントリは最新出現を優先して除去
///
/// JSON パース失敗行はスキップし、ファイル全体の読み込みは続行する。
/// 一方で行読み込み中の IO エラー（NFS 断絶・ディスク EIO 等）は
/// 履歴欠落をサイレントに招くため、呼び出し元に伝播する。
pub fn load_prompts(history_path: &Path) -> std::io::Result<Vec<Prompt>> {
    let paste_cache_dir = history_path
        .parent()
        .map(|p| p.join("paste-cache"))
        .unwrap_or_else(|| PathBuf::from("paste-cache"));
    let file = File::open(history_path)?;
    load_prompts_from_reader_with_cache(BufReader::new(file), &paste_cache_dir)
}

/// `BufRead` から JSONL を読み込む。paste-cache 展開なし（テスト用）。
fn load_prompts_from_reader<R: BufRead>(reader: R) -> std::io::Result<Vec<Prompt>> {
    load_prompts_from_reader_with_cache(reader, Path::new(""))
}

/// `BufRead` から JSONL を読み込む。paste-cache 展開あり。
fn load_prompts_from_reader_with_cache<R: BufRead>(
    reader: R,
    paste_cache_dir: &Path,
) -> std::io::Result<Vec<Prompt>> {
    let lines = reader.lines().collect::<std::io::Result<Vec<_>>>()?;
    Ok(collect_prompts_with_cache(lines.into_iter(), paste_cache_dir))
}

/// Unix ミリ秒タイムスタンプをローカル時刻の ISO 8601 形式文字列に変換する。
///
/// `libc::localtime_r` でローカル時刻に変換する。UTC ではないため末尾に 'Z' を付けない。
/// 「いつ打ったか」を人間が読む用途なのでローカル時刻が適切。
fn unix_ms_to_local_iso(ms: u64) -> String {
    let secs = (ms / 1000) as libc::time_t;
    let mut t: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&secs, &mut t) };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        t.tm_year + 1900,
        t.tm_mon + 1,
        t.tm_mday,
        t.tm_hour,
        t.tm_min,
        t.tm_sec
    )
}

/// `display` 内の `[Pasted text #id ...]` プレースホルダを
/// `paste_cache_dir/{contentHash}.txt` の実ペースト内容に置換した文字列を返す。
///
/// キャッシュファイルが存在しない ID はプレースホルダのまま残す。
/// ID を降順で処理することで先頭側の置換後に後続 ID の位置が変わっても
/// `find` が再スキャンするため問題ない（ただし降順の方が直感的に安全）。
fn expand_pasted_contents(
    display: &str,
    pasted_contents: &HashMap<String, PasteRef>,
    paste_cache_dir: &Path,
) -> String {
    let mut result = display.to_string();
    let mut refs: Vec<&PasteRef> = pasted_contents.values().collect();
    // 後方（高 ID）から処理: 前方置換による後続 ID のインデックスずれを回避
    refs.sort_by_key(|r| Reverse(r.id));
    for paste_ref in refs {
        let cache_path = paste_cache_dir.join(format!("{}.txt", paste_ref.content_hash));
        let content = match std::fs::read_to_string(&cache_path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let content = content.trim_end_matches('\n');
        let prefix = format!("[Pasted text #{}", paste_ref.id);
        if let Some(start) = result.find(&prefix) {
            if let Some(end_offset) = result[start..].find(']') {
                let end = start + end_offset + 1;
                result.replace_range(start..end, content);
            }
        }
    }
    result
}

/// JSONL 1 行から `Prompt` を取り出す。
///
/// 空行・JSON パース失敗・`display` 欠落・空文字列はすべて `None` を返す。
/// `paste_cache_dir` が空文字列のとき paste 展開をスキップする（テスト用）。
fn parse_entry(line: &str, paste_cache_dir: &Path) -> Option<Prompt> {
    if line.trim().is_empty() {
        return None;
    }
    let entry: HistoryEntry = serde_json::from_str(line).ok()?;
    let display = entry.display?.trim().to_string();
    if display.is_empty() {
        return None;
    }
    // isoTimestamp（旧形式）を優先し、なければ timestamp（現行形式）を変換する
    let iso_timestamp = entry
        .iso_timestamp
        .or_else(|| entry.timestamp.map(unix_ms_to_local_iso));
    // paste-cache ディレクトリが指定されていてペーストコンテンツがある場合のみ展開
    let full_text = match &entry.pasted_contents {
        Some(pasted) if !pasted.is_empty() && !paste_cache_dir.as_os_str().is_empty() => {
            expand_pasted_contents(&display, pasted, paste_cache_dir)
        }
        _ => display.clone(),
    };
    Some(Prompt {
        display,
        full_text,
        iso_timestamp,
    })
}

/// fzf 表示候補として採用すべきプロンプトか判定する。
///
/// 除外条件:
/// - 単独スラッシュコマンド形式（`/help` `/code-review` 等）: Claude Code 内
///   で `/` キーからメニュー選択できるため fzf に出す価値が低い。
///   引数や記号を伴うもの（`/loop 5m /foo` `/foo:bar` 等）は手入力が長く
///   再利用価値が高いため採用する。
fn is_eligible(display: &str) -> bool {
    !is_bare_slash_command(display)
}

/// 単独スラッシュコマンド判定: `/` + 英数/ハイフン/アンダースコアのみ。
///
/// Claude Code の slash command 名は ASCII 英数 + `-` + `_` で構成される。
/// この形式に完全一致するものは内蔵 `/` メニューで補えるため fzf 候補から
/// 除外する。判定軸を「空白の有無」ではなく「単独 slash 形式か」に置くこ
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

/// 最新出現を優先して重複除去する（fzf の体感に合わせ末尾優先）。
///
/// 入力は時系列（古い→新しい）を想定。同一 `display` は最後の出現位置だけ残し、
/// 新しい順（新→古）に並び替えて返す。最後の出現 = 最新タイムスタンプを保持する。
fn dedup_keep_last(prompts: Vec<Prompt>) -> Vec<Prompt> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut result = Vec::with_capacity(prompts.len());
    for p in prompts.into_iter().rev() {
        if seen.insert(p.display.clone()) {
            result.push(p);
        }
    }
    result
}

/// JSONL 行イテレータからプロンプトを収集する（paste-cache 展開なし・テスト用）。
///
/// 構成: parse → filter → dedup の 3 段。ファイル I/O を伴わないため失敗しない。
pub fn collect_prompts(lines: impl Iterator<Item = String>) -> Vec<Prompt> {
    collect_prompts_with_cache(lines, Path::new(""))
}

/// JSONL 行イテレータからプロンプトを収集する（paste-cache 展開あり）。
pub fn collect_prompts_with_cache(
    lines: impl Iterator<Item = String>,
    paste_cache_dir: &Path,
) -> Vec<Prompt> {
    let eligible: Vec<Prompt> = lines
        .filter_map(|l| parse_entry(&l, paste_cache_dir))
        .filter(|p| is_eligible(&p.display))
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

    fn displays(prompts: Vec<Prompt>) -> Vec<String> {
        prompts.into_iter().map(|p| p.display).collect()
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
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["ビルドして"]);
    }

    #[test]
    fn iso_timestamp_is_preserved() {
        let input = r#"{"display":"ビルドして","isoTimestamp":"2026-06-03T01:20:13.918Z"}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(
            result[0].iso_timestamp.as_deref(),
            Some("2026-06-03T01:20:13.918Z")
        );
    }

    #[test]
    fn missing_iso_timestamp_is_none() {
        // isoTimestamp も timestamp も存在しない場合は None。
        let input = r#"{"display":"ビルドして"}"#;
        let result = collect_prompts(lines(input));
        assert!(result[0].iso_timestamp.is_none());
    }

    #[test]
    fn numeric_timestamp_is_converted_to_local_iso_string() {
        // 現行 Claude Code は isoTimestamp でなく timestamp（数値 ms）を書く。
        // 1780928372000 ms ≈ 2026-06-08 JST
        let input = r#"{"display":"test","timestamp":1780928372000}"#;
        let result = collect_prompts(lines(input));
        assert!(
            result[0].iso_timestamp.is_some(),
            "timestamp フィールドが iso_timestamp に変換されるべき"
        );
        let ts = result[0].iso_timestamp.as_ref().unwrap();
        assert!(ts.contains("2026"), "2026 年のタイムスタンプになるべき: {ts}");
    }

    #[test]
    fn iso_timestamp_takes_priority_over_numeric_timestamp() {
        // isoTimestamp が存在する場合は timestamp より優先する（後方互換）。
        let input =
            r#"{"display":"test","isoTimestamp":"2026-06-03T01:20:13.918Z","timestamp":1000}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(
            result[0].iso_timestamp.as_deref(),
            Some("2026-06-03T01:20:13.918Z")
        );
    }

    #[test]
    fn full_text_equals_display_when_pasted_contents_empty() {
        let input = r#"{"display":"通常のプロンプト","pastedContents":{}}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result[0].full_text, result[0].display);
    }

    #[test]
    fn full_text_equals_display_when_no_pasted_contents_field() {
        let input = r#"{"display":"通常のプロンプト"}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result[0].full_text, result[0].display);
    }

    #[test]
    fn pasted_content_is_expanded_in_full_text() {
        // pastedContents がある場合、full_text ではプレースホルダを実内容に展開する。
        // display は展開しない（fzf リスト表示用に短縮形を保持する）。
        let dir = tempfile::TempDir::new().unwrap();
        let hash = "deadbeef00000000";
        std::fs::write(dir.path().join(format!("{hash}.txt")), "actual content").unwrap();
        let input = format!(
            r#"{{"display":"before [Pasted text #1 +1 lines] after","pastedContents":{{"1":{{"id":1,"type":"text","contentHash":"{hash}"}}}}}}"#
        );
        let prompts = collect_prompts_with_cache(std::iter::once(input), dir.path());
        assert_eq!(
            prompts[0].display,
            "before [Pasted text #1 +1 lines] after",
            "display はプレースホルダのまま保持する"
        );
        assert_eq!(
            prompts[0].full_text,
            "before actual content after",
            "full_text はキャッシュ内容に展開する"
        );
    }

    #[test]
    fn multiline_pasted_content_is_expanded() {
        // 複数行のペースト内容も正しく展開される。
        let dir = tempfile::TempDir::new().unwrap();
        let hash = "aabbccdd00000000";
        std::fs::write(
            dir.path().join(format!("{hash}.txt")),
            "line1\nline2\nline3\n",
        )
        .unwrap();
        let input = format!(
            r#"{{"display":"prefix [Pasted text #1 +3 lines] suffix","pastedContents":{{"1":{{"id":1,"type":"text","contentHash":"{hash}"}}}}}}"#
        );
        let prompts = collect_prompts_with_cache(std::iter::once(input), dir.path());
        // 末尾の改行は trim_end_matches('\n') で除去される
        assert_eq!(prompts[0].full_text, "prefix line1\nline2\nline3 suffix");
    }

    #[test]
    fn missing_paste_cache_file_keeps_placeholder() {
        // キャッシュファイルが存在しない場合は display をそのまま full_text にする。
        let dir = tempfile::TempDir::new().unwrap();
        let input = r#"{"display":"before [Pasted text #1 +1 lines] after","pastedContents":{"1":{"id":1,"type":"text","contentHash":"nonexistent_hash"}}}"#;
        let prompts =
            collect_prompts_with_cache(std::iter::once(input.to_string()), dir.path());
        assert_eq!(
            prompts[0].full_text,
            "before [Pasted text #1 +1 lines] after",
            "キャッシュ欠落時は display をそのまま使う"
        );
    }

    #[test]
    fn dedup_keeps_latest_timestamp() {
        // 同一 display の重複のうち最新（末尾）の iso_timestamp を保持することを検証する。
        let input = r#"{"display":"重複","isoTimestamp":"2026-01-01T00:00:00Z"}
{"display":"重複","isoTimestamp":"2026-06-03T10:00:00Z"}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].iso_timestamp.as_deref(),
            Some("2026-06-03T10:00:00Z"),
            "最新のタイムスタンプを保持すべき"
        );
    }

    #[test]
    fn bare_slash_command_is_excluded() {
        let input = r#"{"display":"/help"}
{"display":"通常のプロンプト"}"#;
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["通常のプロンプト"]);
    }

    #[test]
    fn slash_command_with_args_is_included() {
        // 引数付き（`/loop 5m /foo` 等）は手入力が長く再利用価値が高いため採用する。
        let input = r#"{"display":"/loop 5m /foo"}
{"display":"/code-review --comment"}
{"display":"/help"}"#;
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["/code-review --comment", "/loop 5m /foo"]);
    }

    #[test]
    fn slash_command_with_tab_is_included() {
        // tab を含む slash command は単独形でないため採用する。
        let input = "{\"display\":\"/foo\\tbar\"}";
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["/foo\tbar"]);
    }

    #[test]
    fn slash_command_with_fullwidth_space_is_included() {
        // 全角スペース (U+3000) 混入の slash command は単独形でないため採用する。
        let input = "{\"display\":\"/loop　5m\"}";
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["/loop\u{3000}5m"]);
    }

    #[test]
    fn slash_command_with_symbol_separator_is_included() {
        // 記号区切り（`:` `;` `|` `=` 等）も単独形でないため採用する。
        let input = r#"{"display":"/foo:bar"}
{"display":"/foo=v"}"#;
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["/foo=v", "/foo:bar"]);
    }

    #[test]
    fn slash_command_with_hyphen_only_is_excluded() {
        // `/code-review` 等のハイフン含む単独形は除外（Claude Code 内 `/`
        // メニューで補える）。
        let input = r#"{"display":"/code-review"}
{"display":"通常のプロンプト"}"#;
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["通常のプロンプト"]);
    }

    #[test]
    fn lone_slash_is_not_treated_as_bare_command() {
        // `/` 単独は slash command 名がないため bare 扱いしない（採用）。
        let input = r#"{"display":"/"}"#;
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["/"]);
    }

    #[test]
    fn duplicate_is_removed_keeping_first() {
        let input = r#"{"display":"重複テスト"}
{"display":"重複テスト"}
{"display":"別のプロンプト"}"#;
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["別のプロンプト", "重複テスト"]);
    }

    #[test]
    fn empty_display_is_excluded() {
        let input = r#"{"display":""}
{"display":"有効なプロンプト"}"#;
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["有効なプロンプト"]);
    }

    #[test]
    fn missing_display_field_is_skipped() {
        let input = r#"{"other_field":"value"}
{"display":"有効"}"#;
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["有効"]);
    }

    #[test]
    fn invalid_json_line_is_skipped() {
        let input = r#"not-json
{"display":"有効"}"#;
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["有効"]);
    }

    #[test]
    fn whitespace_is_trimmed() {
        let input = r#"{"display":"  前後スペース  "}"#;
        let result = displays(collect_prompts(lines(input)));
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
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["line1\nline2"]);
    }

    #[test]
    fn multiline_duplicate_dedup_uses_full_text() {
        let input = r#"{"display":"line1\nline2"}
{"display":"line1\nline2"}
{"display":"line1\nline3"}"#;
        let result = displays(collect_prompts(lines(input)));
        // collect_prompts は最新優先で返すため、ファイル末尾（line3）が先頭に来る
        assert_eq!(result, vec!["line1\nline3", "line1\nline2"]);
    }

    // ─── collect_prompts は full_text == display を常に保証 ──────────────────

    #[test]
    fn collect_prompts_full_text_always_equals_display() {
        // cache なし版では full_text は展開されず display と一致する。
        let input =
            r#"{"display":"before [Pasted text #1 +1 lines] after","pastedContents":{"1":{"id":1,"type":"text","contentHash":"abc"}}}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result[0].full_text, result[0].display);
    }

    // ─── 複数のペースト参照 ────────────────────────────────────────────────────

    #[test]
    fn multiple_paste_refs_are_all_expanded() {
        // 2 つのプレースホルダが両方とも展開される。
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("hash1.txt"), "FIRST_CONTENT").unwrap();
        std::fs::write(dir.path().join("hash2.txt"), "SECOND_CONTENT").unwrap();
        // 高 id から降順展開するため #2 を先に置く（置換位置がずれないよう逆順処理）。
        let input = r#"{"display":"a [Pasted text #1 +0 lines] b [Pasted text #2 +0 lines] c","pastedContents":{"1":{"id":1,"type":"text","contentHash":"hash1"},"2":{"id":2,"type":"text","contentHash":"hash2"}}}"#;
        let prompts = collect_prompts_with_cache(std::iter::once(input.to_string()), dir.path());
        assert_eq!(prompts[0].full_text, "a FIRST_CONTENT b SECOND_CONTENT c");
    }

    #[test]
    fn multiple_paste_refs_display_stays_as_placeholder() {
        // 複数ペーストでも display は展開されない。
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("hash1.txt"), "A").unwrap();
        std::fs::write(dir.path().join("hash2.txt"), "B").unwrap();
        let input = r#"{"display":"[Pasted text #1 +0 lines] [Pasted text #2 +0 lines]","pastedContents":{"1":{"id":1,"type":"text","contentHash":"hash1"},"2":{"id":2,"type":"text","contentHash":"hash2"}}}"#;
        let prompts = collect_prompts_with_cache(std::iter::once(input.to_string()), dir.path());
        assert_eq!(
            prompts[0].display,
            "[Pasted text #1 +0 lines] [Pasted text #2 +0 lines]"
        );
    }

    // ─── dedup は display キーで行い full_text は最新を保持 ──────────────────

    #[test]
    fn dedup_preserves_full_text_of_latest_entry() {
        // 同 display で 2 エントリある場合、後のエントリの full_text が残る。
        // collect_prompts_with_cache を使い、異なる full_text が生成される状況を再現。
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("hash_old.txt"), "OLD_CONTENT").unwrap();
        std::fs::write(dir.path().join("hash_new.txt"), "NEW_CONTENT").unwrap();
        // display は同じ "[Pasted text #1 +0 lines]" だが cache hash が異なる。
        let line1 = r#"{"display":"[Pasted text #1 +0 lines]","pastedContents":{"1":{"id":1,"type":"text","contentHash":"hash_old"}}}"#;
        let line2 = r#"{"display":"[Pasted text #1 +0 lines]","pastedContents":{"1":{"id":1,"type":"text","contentHash":"hash_new"}}}"#;
        let prompts = collect_prompts_with_cache(
            vec![line1.to_string(), line2.to_string()].into_iter(),
            dir.path(),
        );
        assert_eq!(prompts.len(), 1, "重複は 1 件に圧縮される");
        assert_eq!(
            prompts[0].full_text,
            "NEW_CONTENT",
            "最新エントリの full_text が保持される"
        );
    }

    // ─── 空の cache_dir は展開をスキップして display をそのまま使う ────────

    #[test]
    fn empty_cache_dir_path_skips_expansion() {
        // collect_prompts（cache なし版）は paste_cache_dir を Path::new("") で呼ぶ。
        // pastedContents が存在しても展開されずに display == full_text になること。
        let input = r#"{"display":"[Pasted text #1 +2 lines]","pastedContents":{"1":{"id":1,"type":"text","contentHash":"abc"}}}"#;
        let result = collect_prompts_with_cache(std::iter::once(input.to_string()), Path::new(""));
        assert_eq!(result[0].full_text, result[0].display);
    }
}
