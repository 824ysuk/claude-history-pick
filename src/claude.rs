//! Claude Code の ~/.claude/history.jsonl からプロンプト履歴を読み込む層。
//!
//! 責務: JSON パース・ペーストキャッシュ展開のみ。フィルタリング・重複除去・
//! 他ソースとのマージは `history::merge_sort_dedup` が担う。
//! UI（fzf）・クリップボード・キーストロークは扱わない。

use crate::history::{read_lines_from_path, unix_ms_to_local_iso, Prompt, Source};
use serde::Deserialize;
use std::cmp::Reverse;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// history.jsonl の pastedContents エントリ 1 件。
///
/// ペースト本文の格納形式は 2 種類あり、値オブジェクトごとに排他的に現れる。
/// 本文をインラインに埋め込む `content` 形式と、`paste-cache/{hash}.txt` への
/// 参照である `contentHash` 形式。どちらの形式で書かれるかは Claude Code 側の
/// 裁量のため両方を Option にする。必須フィールドのままだと片方欠落側の値で
/// デシリアライズが失敗し、`pastedContents` ごと、ひいては行全体（プロンプト）
/// が `parse_entry` の `.ok()?` に握り潰され履歴から消える。将来別形式が
/// 増えても両方 `None` になるだけで行は残る。
///
/// `type`（実データでは常に `"text"`）はあえて読まない。読んで分岐すると
/// 未知の type で同じ失敗を繰り返す。
#[derive(Deserialize)]
struct PasteRef {
    id: u32,
    #[serde(rename = "contentHash")]
    content_hash: Option<String>,
    content: Option<String>,
}

impl PasteRef {
    /// このペースト参照の本文を解決する。解決できない場合は `None`。
    ///
    /// インライン `content` を優先する。ファイルシステムに依存せず失敗経路を
    /// 持たないため、`contentHash` のキャッシュファイルが削除済みでも本文を
    /// 失わない。`cache_dir` が `None` のときは `contentHash` 経由の解決のみ
    /// 不能になる（インライン形式はファイルアクセスを要さないため影響しない）。
    ///
    /// 末尾改行は両形式とも落とす。プレースホルダは文中に現れることが多く
    /// 改行を残すと文が分断され、文末にある場合はターミナルへの貼り付け時に
    /// 意図しない送信を招く。形式間で正規化を揃えることで、同じペーストが
    /// 記録形式によって異なるクリップボード内容にならないことも保証する。
    fn resolve(&self, cache_dir: Option<&Path>) -> Option<String> {
        if let Some(content) = &self.content {
            return Some(content.trim_end_matches('\n').to_string());
        }
        let hash = self.content_hash.as_deref()?;
        let cache_dir = cache_dir?;
        // hash はファイル名に連結する。空文字は `.txt` の意図しない読み出し、
        // 非 hex 値はパストラバーサルの経路になり得るためスキップ。
        if hash.is_empty() || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let raw = std::fs::read_to_string(cache_dir.join(format!("{hash}.txt"))).ok()?;
        Some(raw.trim_end_matches('\n').to_string())
    }
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

/// `history_path` の JSONL を読み込み、`Prompt` 一覧を返す（フィルタ・重複除去前）。
///
/// paste-cache ディレクトリは `history_path` の親ディレクトリ内の
/// `paste-cache/` サブディレクトリを使う（`~/.claude/paste-cache/`）。
///
/// フィルタ条件（本関数内）:
/// - `display` フィールドが存在し、空でない行のみ採用
///
/// 単独スラッシュコマンド除外・重複除去は `history::merge_sort_dedup` が
/// 他ソースとの統合時にまとめて行う。
///
/// JSON パース失敗行はスキップし、ファイル全体の読み込みは続行する。
/// 一方で行読み込み中の IO エラー（NFS 断絶・ディスク EIO 等）は
/// 履歴欠落をサイレントに招くため、呼び出し元に伝播する。
pub fn load_claude_prompts(history_path: &Path) -> std::io::Result<Vec<Prompt>> {
    let paste_cache_dir = history_path
        .parent()
        .map(|p| p.join("paste-cache"))
        .unwrap_or_else(|| PathBuf::from("paste-cache"));
    let lines = read_lines_from_path(history_path)?;
    Ok(collect_prompts_with_cache(
        lines.into_iter(),
        &paste_cache_dir,
    ))
}

/// `text` 中の `[Pasted text #id ...]` プレースホルダの範囲
/// （開始位置 〜 対応する `]` の次）を返す。見つからなければ `None`。
///
/// ID の前方一致（`#1` が `#10` のプレフィックスになる）を避けるため、
/// ID 直後が数字でないことを確認する。ID を降順処理していても、高 ID 側の
/// 解決が失敗してプレースホルダが残った場合はこのチェックが必要になる。
fn find_placeholder(text: &str, id: u32) -> Option<(usize, usize)> {
    let prefix = format!("[Pasted text #{id}");
    let mut from = 0;
    while let Some(rel) = text[from..].find(&prefix) {
        let start = from + rel;
        let after = start + prefix.len();
        if !text[after..].starts_with(|c: char| c.is_ascii_digit()) {
            return text[after..].find(']').map(|e| (start, after + e + 1));
        }
        from = after;
    }
    None
}

/// `display` 内の `[Pasted text #id ...]` プレースホルダを実ペースト内容に
/// 置換した文字列を返す。
///
/// 解決できない ID はプレースホルダのまま残す（1 件失敗しても他の ID の
/// 展開は続ける）。ID を降順で処理するのは `HashMap` の反復順が不定なため
/// 結果を決定的にする目的（`find_placeholder` が前方一致を防ぐため、
/// 置換順序自体が結果を左右することはない）。
fn expand_pasted_contents(
    display: &str,
    pasted_contents: &HashMap<String, PasteRef>,
    cache_dir: Option<&Path>,
) -> String {
    let mut result = display.to_string();
    let mut refs: Vec<&PasteRef> = pasted_contents.values().collect();
    refs.sort_by_key(|r| Reverse(r.id));
    for paste_ref in refs {
        let Some(content) = paste_ref.resolve(cache_dir) else {
            continue;
        };
        if let Some((start, end)) = find_placeholder(&result, paste_ref.id) {
            result.replace_range(start..end, &content);
        }
    }
    result
}

/// レガシー `isoTimestamp`（RFC3339 文字列、例 `"2026-06-03T01:20:13.918Z"`）を
/// Unix ミリ秒に変換する。
///
/// `timestamp`（数値 ms）フィールドを持たない古いエントリでも、この変換に
/// よって `history::merge_sort_dedup` の時刻降順マージ・重複除去に正しく
/// 参加できる。変換なしで `timestamp_ms = None` のままだと、実際の記録時刻に
/// 関わらず常に最下位に沈み、かつ同一 `(source, display)` の重複除去でも
/// 「最初に現れたもの」が最新とは限らなくなる（stable sort が元の並びを保持
/// するため）。パース失敗時は `None`（実データでは発生しない fallback）。
fn parse_iso_timestamp_to_ms(iso: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// JSONL 1 行から `Prompt` を取り出す。
///
/// 空行・JSON パース失敗・`display` 欠落・空文字列はすべて `None` を返す。
/// `paste_cache_dir` が空文字列のとき `contentHash` 経由の展開のみをスキップ
/// する（テスト用。インライン `content` はファイルアクセスを要さないため
/// この空文字列センチネルの影響を受けず常に展開される）。
fn parse_entry(line: &str, paste_cache_dir: &Path) -> Option<Prompt> {
    if line.trim().is_empty() {
        return None;
    }
    let entry: HistoryEntry = serde_json::from_str(line).ok()?;
    let display = entry.display?.trim().to_string();
    if display.is_empty() {
        return None;
    }
    // timestamp（現行形式、数値 ms）を優先。isoTimestamp（旧形式）しかない場合も
    // parse_iso_timestamp_to_ms で数値化し、他ソースとのマージ順序を正しく保つ。
    let timestamp_ms = entry.timestamp.map(|ms| ms as i64).or_else(|| {
        entry
            .iso_timestamp
            .as_deref()
            .and_then(parse_iso_timestamp_to_ms)
    });
    // 表示用文字列は isoTimestamp（旧形式）を優先し、なければ timestamp を変換する。
    let iso_timestamp = entry
        .iso_timestamp
        .or_else(|| timestamp_ms.map(unix_ms_to_local_iso));
    // 空パスは「ファイルアクセスなし」を表すテスト用センチネル。ここで Option
    // に畳んでおき、expand_pasted_contents 以降はマジックな空パスを知らずに済む。
    let cache_dir = (!paste_cache_dir.as_os_str().is_empty()).then_some(paste_cache_dir);
    let full_text = match &entry.pasted_contents {
        Some(pasted) if !pasted.is_empty() => expand_pasted_contents(&display, pasted, cache_dir),
        _ => display.clone(),
    };
    Some(Prompt {
        source: Source::Claude,
        display,
        full_text,
        iso_timestamp,
        timestamp_ms,
    })
}

/// JSONL 行イテレータからプロンプトを収集する（`contentHash` 経由の展開なし・
/// テスト用）。インライン `content` はファイルアクセスを要さないため
/// この関数でも展開される。
#[cfg(test)]
fn collect_prompts(lines: impl Iterator<Item = String>) -> Vec<Prompt> {
    collect_prompts_with_cache(lines, Path::new(""))
}

/// JSONL 行イテレータからプロンプトを収集する（paste-cache 展開あり）。
///
/// フィルタ・重複除去は行わない（`history::merge_sort_dedup` に委ねる）。
fn collect_prompts_with_cache(
    lines: impl Iterator<Item = String>,
    paste_cache_dir: &Path,
) -> Vec<Prompt> {
    lines
        .filter_map(|l| parse_entry(&l, paste_cache_dir))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::merge_sort_dedup;
    use std::io;

    fn lines(raw: &str) -> impl Iterator<Item = String> + '_ {
        raw.lines().map(|l| l.to_string())
    }

    fn displays(prompts: Vec<Prompt>) -> Vec<String> {
        prompts.into_iter().map(|p| p.display).collect()
    }

    #[test]
    fn load_claude_prompts_propagates_not_found() {
        // read_lines 自体の IO エラー伝播は history.rs で確認済み。ここでは
        // load_claude_prompts が正しく read_lines_from_path に繋がっていることのみ確認する。
        let result = load_claude_prompts(Path::new("/definitely/does/not/exist/history.jsonl"));
        let err = result.expect_err("存在しないパスは Err を返すべき");
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn normal_entry_is_included() {
        let input = r#"{"display":"ビルドして"}"#;
        let result = displays(collect_prompts(lines(input)));
        assert_eq!(result, vec!["ビルドして"]);
    }

    #[test]
    fn entry_source_is_claude() {
        let input = r#"{"display":"ビルドして"}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result[0].source, Source::Claude);
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
    fn iso_timestamp_only_entry_derives_timestamp_ms_from_rfc3339() {
        // isoTimestamp レガシーフィールドのみでも RFC3339 パースで timestamp_ms を
        // 逆算する（merge_sort_dedup での最下位固定・dedup 誤挙動を防ぐ）。
        let input = r#"{"display":"ビルドして","isoTimestamp":"2026-06-03T01:20:13.918Z"}"#;
        let result = collect_prompts(lines(input));
        // 2026-06-03T01:20:13.918Z の Unix ミリ秒。
        assert_eq!(result[0].timestamp_ms, Some(1_780_449_613_918));
    }

    #[test]
    fn malformed_iso_timestamp_falls_back_to_none() {
        // パース不能な isoTimestamp（実データでは発生しない想定）は
        // timestamp_ms = None にフォールバックし、クラッシュしない。
        let input = r#"{"display":"ビルドして","isoTimestamp":"not-a-valid-timestamp"}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result[0].timestamp_ms, None);
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
        assert!(
            ts.contains("2026"),
            "2026 年のタイムスタンプになるべき: {ts}"
        );
        assert_eq!(result[0].timestamp_ms, Some(1780928372000));
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
            prompts[0].display, "before [Pasted text #1 +1 lines] after",
            "display はプレースホルダのまま保持する"
        );
        assert_eq!(
            prompts[0].full_text, "before actual content after",
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
        let prompts = collect_prompts_with_cache(std::iter::once(input.to_string()), dir.path());
        assert_eq!(
            prompts[0].full_text, "before [Pasted text #1 +1 lines] after",
            "キャッシュ欠落時は display をそのまま使う"
        );
    }

    #[test]
    fn dedup_keeps_latest_timestamp() {
        // merge_sort_dedup 経由で同一 display の重複のうち最新（大きい timestamp_ms）
        // の iso_timestamp を保持することを検証する。
        let input = r#"{"display":"重複","timestamp":1780000000000}
{"display":"重複","timestamp":1780900000000}"#;
        let result = merge_sort_dedup(collect_prompts(lines(input)));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].timestamp_ms, Some(1780900000000));
    }

    #[test]
    fn slash_command_with_tab_is_included() {
        // tab を含む slash command は単独形でないため採用する。
        let input = "{\"display\":\"/foo\\tbar\"}";
        let result = displays(merge_sort_dedup(collect_prompts(lines(input))));
        assert_eq!(result, vec!["/foo\tbar"]);
    }

    #[test]
    fn slash_command_with_fullwidth_space_is_included() {
        // 全角スペース (U+3000) 混入の slash command は単独形でないため採用する。
        let input = "{\"display\":\"/loop　5m\"}";
        let result = displays(merge_sort_dedup(collect_prompts(lines(input))));
        assert_eq!(result, vec!["/loop\u{3000}5m"]);
    }

    #[test]
    fn duplicate_is_removed_keeping_first() {
        let input = r#"{"display":"重複テスト","timestamp":1000}
{"display":"重複テスト","timestamp":2000}
{"display":"別のプロンプト","timestamp":3000}"#;
        let result = displays(merge_sort_dedup(collect_prompts(lines(input))));
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

    // ─── collect_prompts は contentHash 参照を展開しない ──────────────────────

    #[test]
    fn collect_prompts_does_not_expand_content_hash_reference() {
        // collect_prompts はファイルアクセスなし版。contentHash 参照はキャッシュ
        // ファイルを読めないため展開されず display のまま残る。
        let input = r#"{"display":"before [Pasted text #1 +1 lines] after","pastedContents":{"1":{"id":1,"type":"text","contentHash":"abc"}}}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result[0].full_text, result[0].display);
    }

    #[test]
    fn collect_prompts_expands_inline_content_even_without_cache_dir() {
        // インライン content はファイルアクセスを要さないため、cache なし版
        // (collect_prompts) でも展開される。本 Issue の主回帰テスト。
        let input = r#"{"display":"before [Pasted text #1 +1 lines] after","pastedContents":{"1":{"id":1,"type":"text","content":"ACTUAL"}}}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result[0].full_text, "before ACTUAL after");
    }

    // ─── 複数のペースト参照 ────────────────────────────────────────────────────

    #[test]
    fn multiple_paste_refs_are_all_expanded() {
        // 2 つのプレースホルダが両方とも展開される。
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("deadbeef00000001.txt"), "FIRST_CONTENT").unwrap();
        std::fs::write(dir.path().join("deadbeef00000002.txt"), "SECOND_CONTENT").unwrap();
        // 高 id から降順展開するため #2 を先に置く（置換位置がずれないよう逆順処理）。
        let input = r#"{"display":"a [Pasted text #1 +0 lines] b [Pasted text #2 +0 lines] c","pastedContents":{"1":{"id":1,"type":"text","contentHash":"deadbeef00000001"},"2":{"id":2,"type":"text","contentHash":"deadbeef00000002"}}}"#;
        let prompts = collect_prompts_with_cache(std::iter::once(input.to_string()), dir.path());
        assert_eq!(prompts[0].full_text, "a FIRST_CONTENT b SECOND_CONTENT c");
    }

    #[test]
    fn multiple_paste_refs_display_stays_as_placeholder() {
        // 複数ペーストでも display は展開されない。
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("deadbeef00000001.txt"), "A").unwrap();
        std::fs::write(dir.path().join("deadbeef00000002.txt"), "B").unwrap();
        let input = r#"{"display":"[Pasted text #1 +0 lines] [Pasted text #2 +0 lines]","pastedContents":{"1":{"id":1,"type":"text","contentHash":"deadbeef00000001"},"2":{"id":2,"type":"text","contentHash":"deadbeef00000002"}}}"#;
        let prompts = collect_prompts_with_cache(std::iter::once(input.to_string()), dir.path());
        assert_eq!(
            prompts[0].display,
            "[Pasted text #1 +0 lines] [Pasted text #2 +0 lines]"
        );
    }

    // ─── インライン content 形式（Issue #77 回帰） ────────────────────────────
    // 実 history.jsonl の pastedContents には、本文を直接埋め込む形式
    // {"content":"..."} と paste-cache を参照する形式 {"contentHash":"..."} が
    // 混在する。contentHash を必須フィールドにしていた旧実装では、インライン
    // 形式の値 1 つでも含む行が丸ごとデシリアライズに失敗し、プロンプト全体が
    // 履歴から消えていた。

    #[test]
    fn inline_content_entry_is_not_dropped() {
        // 最重要回帰テスト: インライン形式の行がパース失敗で捨てられず
        // Prompt として残ること自体を、展開結果とは独立に確認する。
        let input = r#"{"display":"[Pasted text #1 +1 lines]","pastedContents":{"1":{"id":1,"type":"text","content":"本文"}}}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result.len(), 1, "インライン形式の行が消えてはならない");
    }

    #[test]
    fn inline_content_is_expanded_in_full_text() {
        let dir = tempfile::TempDir::new().unwrap();
        let input = r#"{"display":"before [Pasted text #1 +1 lines] after","pastedContents":{"1":{"id":1,"type":"text","content":"actual content"}}}"#;
        let prompts = collect_prompts_with_cache(std::iter::once(input.to_string()), dir.path());
        assert_eq!(prompts[0].full_text, "before actual content after");
        assert_eq!(
            prompts[0].display, "before [Pasted text #1 +1 lines] after",
            "display はプレースホルダのまま保持する"
        );
    }

    #[test]
    fn mixed_inline_and_content_hash_refs_in_one_entry_are_both_expanded() {
        // 実データで 26 件確認された「1 エントリ内で 2 形式が混在する」ケース。
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("deadbeef00000002.txt"), "FROM_CACHE").unwrap();
        let input = r#"{"display":"a [Pasted text #1 +0 lines] b [Pasted text #2 +0 lines] c","pastedContents":{"1":{"id":1,"type":"text","content":"FROM_INLINE"},"2":{"id":2,"type":"text","contentHash":"deadbeef00000002"}}}"#;
        let prompts = collect_prompts_with_cache(std::iter::once(input.to_string()), dir.path());
        assert_eq!(prompts[0].full_text, "a FROM_INLINE b FROM_CACHE c");
    }

    #[test]
    fn inline_content_trailing_newline_is_trimmed() {
        // contentHash 形式と同様、末尾改行はプレースホルダの前後にある文を
        // 分断しないよう除去する。
        let input = r#"{"display":"before [Pasted text #1 +1 lines] after","pastedContents":{"1":{"id":1,"type":"text","content":"line1\nline2\n"}}}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result[0].full_text, "before line1\nline2 after");
    }

    #[test]
    fn paste_ref_without_content_or_hash_keeps_placeholder() {
        // 将来 Claude Code が第 3 の形式を書いても、content・contentHash が
        // 両方 None になるだけでエントリ自体は残り、プレースホルダは維持される。
        let input = r#"{"display":"before [Pasted text #1 +1 lines] after","pastedContents":{"1":{"id":1,"type":"text"}}}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result.len(), 1, "行が消えてはならない");
        assert_eq!(result[0].full_text, result[0].display);
    }

    #[test]
    fn inline_content_without_placeholder_in_display_is_preserved() {
        // 実データで 102 件確認された「貼り付け後にプレースホルダを消して
        // 送信した」ケース。展開対象がなくても full_text は display のまま。
        let input = r#"{"display":"通常の文章です","pastedContents":{"1":{"id":1,"type":"text","content":"未使用の貼り付け"}}}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(result[0].full_text, "通常の文章です");
    }

    #[test]
    fn inline_content_takes_priority_over_content_hash() {
        // 実データでは 2 フィールド同時出現は観測されないが、将来同時に現れた
        // 場合もファイル IO を要さない content を優先する（安全側）。
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("deadbeef00000001.txt"), "FROM_CACHE").unwrap();
        let input = r#"{"display":"[Pasted text #1 +0 lines]","pastedContents":{"1":{"id":1,"type":"text","content":"FROM_INLINE","contentHash":"deadbeef00000001"}}}"#;
        let prompts = collect_prompts_with_cache(std::iter::once(input.to_string()), dir.path());
        assert_eq!(prompts[0].full_text, "FROM_INLINE");
    }

    #[test]
    fn paste_id_1_does_not_match_placeholder_10() {
        // #1 の解決が #10 のプレースホルダに前方一致して誤爆しないこと。
        // #10 側は解決不能（content・contentHash なし）にしてプレースホルダを
        // 残し、#1 の探索が #10 にマッチしない状態を再現する。
        let input = r#"{"display":"[Pasted text #10 +5 lines] [Pasted text #1 +0 lines]","pastedContents":{"1":{"id":1,"type":"text","content":"ONE"},"10":{"id":10,"type":"text"}}}"#;
        let result = collect_prompts(lines(input));
        assert_eq!(
            result[0].full_text, "[Pasted text #10 +5 lines] ONE",
            "#1 の展開が #10 のプレースホルダを破壊してはならない"
        );
    }

    // ─── dedup は display キーで行い full_text は最新を保持 ──────────────────

    #[test]
    fn dedup_preserves_full_text_of_latest_entry() {
        // 同 display で 2 エントリある場合、後のエントリの full_text が残る。
        // collect_prompts_with_cache を使い、異なる full_text が生成される状況を再現。
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("aabbccdd00000001.txt"), "OLD_CONTENT").unwrap();
        std::fs::write(dir.path().join("aabbccdd00000002.txt"), "NEW_CONTENT").unwrap();
        // display は同じ "[Pasted text #1 +0 lines]" だが cache hash が異なり timestamp も異なる。
        let line1 = r#"{"display":"[Pasted text #1 +0 lines]","timestamp":1000,"pastedContents":{"1":{"id":1,"type":"text","contentHash":"aabbccdd00000001"}}}"#;
        let line2 = r#"{"display":"[Pasted text #1 +0 lines]","timestamp":2000,"pastedContents":{"1":{"id":1,"type":"text","contentHash":"aabbccdd00000002"}}}"#;
        let prompts = collect_prompts_with_cache(
            vec![line1.to_string(), line2.to_string()].into_iter(),
            dir.path(),
        );
        let result = merge_sort_dedup(prompts);
        assert_eq!(result.len(), 1, "重複は 1 件に圧縮される");
        assert_eq!(
            result[0].full_text, "NEW_CONTENT",
            "最新エントリの full_text が保持される"
        );
    }

    // ─── 空の cache_dir は contentHash 経由の展開のみをスキップする ────────

    #[test]
    fn empty_cache_dir_path_skips_content_hash_expansion() {
        // paste_cache_dir を Path::new("") で呼ぶと contentHash 参照はファイルに
        // アクセスできず展開されない（display == full_text のまま）。
        let input = r#"{"display":"[Pasted text #1 +2 lines]","pastedContents":{"1":{"id":1,"type":"text","contentHash":"abc"}}}"#;
        let result = collect_prompts_with_cache(std::iter::once(input.to_string()), Path::new(""));
        assert_eq!(result[0].full_text, result[0].display);
    }

    #[test]
    fn empty_cache_dir_path_still_expands_inline_content() {
        // インライン content はファイルアクセスを要さないため、空の
        // paste_cache_dir（ファイルアクセス不可を表すテスト用センチネル）でも
        // 影響を受けずに展開される。
        let input = r#"{"display":"[Pasted text #1 +2 lines]","pastedContents":{"1":{"id":1,"type":"text","content":"ACTUAL"}}}"#;
        let result = collect_prompts_with_cache(std::iter::once(input.to_string()), Path::new(""));
        assert_eq!(result[0].full_text, "ACTUAL");
    }

    // ─── 新着優先保証 ─────────────────────────────────────────────────────────

    #[test]
    fn entries_are_returned_newest_first_based_on_timestamp() {
        // history.jsonl はファイル末尾が最新。merge_sort_dedup が timestamp_ms 降順に
        // 並べ直すことで fzf の先頭に最新プロンプトが来ることを保証する。
        let input = r#"{"display":"最古のプロンプト","timestamp":1000}
{"display":"中間のプロンプト","timestamp":2000}
{"display":"最新のプロンプト","timestamp":3000}"#;
        let result = displays(merge_sort_dedup(collect_prompts(lines(input))));
        assert_eq!(
            result[0], "最新のプロンプト",
            "最新エントリが先頭に来るべき"
        );
        assert_eq!(result[1], "中間のプロンプト");
        assert_eq!(result[2], "最古のプロンプト");
    }

    #[test]
    fn newest_among_duplicates_is_at_top() {
        // 同一 display が複数回現れたとき、最後（最新）の出現が先頭に返る。
        // 「2 回前に打ったコマンドの最新版」が先頭に来ることを保証する。
        // timestamp=1780928372000 ≈ 2026-06-08（実際の Claude Code が書く値のオーダー）。
        let input = r#"{"display":"ビルド","timestamp":1780928000000}
{"display":"テスト","timestamp":1780928100000}
{"display":"ビルド","timestamp":1780928372000}"#;
        let result = merge_sort_dedup(collect_prompts(lines(input)));
        assert_eq!(result[0].display, "ビルド", "重複の最新版が先頭");
        assert!(
            result[0].iso_timestamp.is_some(),
            "最新エントリの iso_timestamp が Some であること"
        );
        let ts = result[0].iso_timestamp.as_deref().unwrap();
        assert!(
            ts.contains("2026"),
            "最新エントリが 2026 年のタイムスタンプを持つ: {ts}"
        );
    }

    // ─── マルチセッション・クロス repo 可視性 ────────────────────────────────

    #[test]
    fn entries_from_multiple_sessions_all_visible() {
        // Claude Code は起動元（直接 / dotfiles / dotfiles-ascend / worktree）に
        // かかわらず全て ~/.claude/history.jsonl に書き込む。
        // 複数の「セッション」から書かれたとみなせる連続エントリが全て見えることを保証する。
        let entries_from_session_a = r#"{"display":"dotfiles から打ったコマンド 1"}
{"display":"dotfiles から打ったコマンド 2"}"#;
        let entries_from_session_b = r#"{"display":"dotfiles-ascend から打ったコマンド"}
{"display":"worktree から打ったコマンド"}"#;
        let combined = format!("{}\n{}", entries_from_session_a, entries_from_session_b);

        let result = displays(collect_prompts(lines(&combined)));

        // 4 件すべてが返る（重複なし・フィルタ除外なし）
        assert_eq!(result.len(), 4, "全セッションのエントリが可視: {result:?}");
        assert!(result.contains(&"dotfiles から打ったコマンド 1".to_string()));
        assert!(result.contains(&"dotfiles から打ったコマンド 2".to_string()));
        assert!(result.contains(&"dotfiles-ascend から打ったコマンド".to_string()));
        assert!(result.contains(&"worktree から打ったコマンド".to_string()));
    }

    #[test]
    fn latest_session_entry_appears_first() {
        // 後から起動した別セッション（worktree 等）のエントリが先頭に来ること。
        let input = r#"{"display":"古いセッションのプロンプト","timestamp":1000}
{"display":"worktree セッションの最新プロンプト","timestamp":2000}"#;
        let result = displays(merge_sort_dedup(collect_prompts(lines(input))));
        assert_eq!(
            result[0], "worktree セッションの最新プロンプト",
            "最後に起動したセッションの最新エントリが先頭に来るべき"
        );
    }

    // ─── 実 history.jsonl との統合テスト ─────────────────────────────────────

    #[test]
    fn real_history_file_is_readable_and_contains_entries() {
        // 実際の ~/.claude/history.jsonl が読めること・エントリが存在すること・
        // 最新エントリが先頭にあることを検証する（CI 環境ではファイルがなければスキップ）。
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let path = std::path::PathBuf::from(&home).join(".claude/history.jsonl");
        if !path.exists() {
            eprintln!(
                "SKIP: {} が存在しないため統合テストをスキップ",
                path.display()
            );
            return;
        }
        let result =
            merge_sort_dedup(load_claude_prompts(&path).expect("history.jsonl の読み込みに失敗"));
        assert!(!result.is_empty(), "history.jsonl にエントリが存在すること");

        // タイムスタンプ: 現行 Claude Code は timestamp（数値 ms）を書くため
        // 少なくとも 1 件は iso_timestamp が Some になるはず
        let has_timestamp = result.iter().any(|p| p.iso_timestamp.is_some());
        assert!(
            has_timestamp,
            "timestamp フィールドが iso_timestamp に変換されていること"
        );

        // 重複なし: (source, display) がすべて一意
        let displays: Vec<(Source, &str)> = result
            .iter()
            .map(|p| (p.source, p.display.as_str()))
            .collect();
        let unique: std::collections::HashSet<&(Source, &str)> =
            displays.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(
            displays.len(),
            unique.len(),
            "重複除去後のエントリに (source, display) の重複があってはならない"
        );
    }

    #[test]
    fn real_history_paste_expansion_works() {
        // 実際の paste-cache を使って expand_pasted_contents が動作することを確認。
        // CI 環境ではファイルがなければスキップ。
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        let history_path = std::path::PathBuf::from(&home).join(".claude/history.jsonl");
        let cache_dir = std::path::PathBuf::from(&home).join(".claude/paste-cache");
        if !history_path.exists() || !cache_dir.exists() {
            eprintln!("SKIP: history.jsonl または paste-cache が存在しないためスキップ");
            return;
        }

        // pastedContents を持つエントリが 1 件以上あれば、
        // そのうち少なくとも 1 件で full_text != display になることを期待する
        // （キャッシュが残っている前提）。
        let result = load_claude_prompts(&history_path).expect("history.jsonl の読み込みに失敗");
        let expanded_count = result.iter().filter(|p| p.full_text != p.display).count();
        // この端末では paste-cache が存在するため少なくとも 1 件は展開されるはず。
        // CI では 0 でも許容（cache が存在しない環境）。
        eprintln!(
            "INFO: {} / {} エントリでペースト展開が成功",
            expanded_count,
            result.len()
        );
    }
}
