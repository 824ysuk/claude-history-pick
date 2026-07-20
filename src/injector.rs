//! Zed へのキーストローク注入層（setsid + osascript）。
//!
//! 責務: Zed のプロセスグループから切り離され、タスクターミナル終了後に
//! cmd-r（terminal::Paste）を発火させる独立プロセスの生成のみ。
//! クリップボードや履歴パースは扱わない。
//!
//! ## なぜ setsid が必要か
//!
//! Zed は `hide: on_success` でタスクターミナルタブを閉じるとき、
//! そのターミナルが属するプロセスグループ全体に SIGTERM を送る。
//!
//! Command::pre_exec で setsid() を呼ぶことで osascript を新しいセッションに移動させ、
//! Zed の SIGTERM から切り離す。double-fork より実装がシンプルで、
//! std の安全なラッパーを使うため exec 失敗を追いやすい。
//!
//! ## フォーカス競合の排除
//!
//! 固定 sleep で「タスクターミナルが閉じてフォーカスが戻るまで待つ」アプローチは
//! マシン負荷によってレースコンディションが発生する。代わりに AppleScript の
//! ポーリングループで Zed が実際に前面に来たことを確認してから cmd-r を送る。
//! これにより固定 sleep への依存を排除する。
//!
//! ## cmd-r を直接送る理由
//!
//! hide: on_success でタスクターミナルが閉じると、フォーカスは元の Claude Code
//! ターミナルに戻る。terminal_panel::ToggleFocus を挟むと、ターミナルが既に
//! フォーカスされている場合にエディタ側に移ってしまう（toggle の副作用）。
//! そのため cmd-r（terminal::Paste）を直接送る。
//!
//! ## Accessibility 権限エラーの検知
//!
//! macOS の TCC で Accessibility 権限が未付与の場合、keystroke は
//! `osascript is not allowed to send keystrokes` エラーで失敗する。
//! AppleScript の `try ... on error ... end try` でこれをキャッチし、
//! macOS 通知でアクセシビリティ設定を案内する。
//! osascript の stderr は /tmp/<uid>.agent-history-pick.osascript.log に記録する。

use crate::secure_log;
use crate::tmp_paths::uid_scoped_tmp_path;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// osascript の stderr ログファイルパス。
///
/// Accessibility 拒否時の `osascript is not allowed to send keystrokes` エラーを記録する。
fn osascript_log_path() -> PathBuf {
    uid_scoped_tmp_path("osascript.log")
}

/// `path` を stderr リダイレクト先として open する。symlink 攻撃対策・権限強制は
/// `secure_log::open_hardened` に委ねる（debug_log.rs と共通の方針）。
///
/// open 失敗時（symlink 経由の拒否を含む）は /dev/null にフォールバックし、
/// 本機能を止めない。
fn open_stderr_log(path: &Path) -> Stdio {
    secure_log::open_hardened(path)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null())
}

/// osascript に渡す `-e` 引数のリストを構築する（純粋関数）。
///
/// 各要素が `osascript -e <element>` の 1 行に対応する。
/// ポーリング設計: 40 回 × 0.05s = 最大 2s で Zed フォーカス取得を確認する。
/// フォーカス確定後 0.3s 安定待ちで keystroke を送る（Zed 入力受付前の race 防止）。
/// これらの値は Zed 実機動作から導いた設計値（環境依存でなく設計上の余裕値）。
fn build_script_args(initial_delay: Duration) -> Vec<String> {
    let delay_secs = initial_delay.as_secs_f64();
    vec![
        format!("delay {delay_secs:.3}"),
        "tell application \"Zed\" to activate".to_string(),
        // Zed が実際に前面に来るまでポーリング（最大 2 秒 = 0.05s × 40）
        "set maxAttempts to 40".to_string(),
        "set gotFocus to false".to_string(),
        "repeat maxAttempts times".to_string(),
        "delay 0.05".to_string(),
        "tell application \"System Events\"".to_string(),
        "if (name of first process whose frontmost is true) is \"Zed\" then".to_string(),
        "set gotFocus to true".to_string(),
        "exit repeat".to_string(),
        "end if".to_string(),
        "end tell".to_string(),
        "end repeat".to_string(),
        "if gotFocus then".to_string(),
        "delay 0.3".to_string(),
        // try/on error で Accessibility 権限エラーをキャッチする。
        // TCC が keystroke を拒否すると -1719 (errAEEventNotPermitted) が返る。
        "try".to_string(),
        "tell application \"System Events\"".to_string(),
        "keystroke \"r\" using command down".to_string(),
        "end tell".to_string(),
        "on error errMsg number errNum".to_string(),
        "display notification \"システム設定 → プライバシーとセキュリティ → アクセシビリティ でターミナル/Zed を許可してください。クリップボードへのコピーは成功しています。\" with title \"agent-history-pick ⚠ Accessibility 権限\"".to_string(),
        "end try".to_string(),
        "else".to_string(),
        "display notification \"Zed がフォーカスを取り戻せませんでした。クリップボードに内容はコピー済みです。手動で cmd-r を押してください。\" with title \"agent-history-pick ⚠\"".to_string(),
        "end if".to_string(),
    ]
}

/// osascript を新しいセッションで起動し、Zed がフォーカスを取り戻した後に cmd-r を送る。
///
/// `initial_delay` はタスクターミナルが閉じ始めるのを待つ最小時間。
/// その後 AppleScript のポーリングで Zed が前面になるまで待機するため、
/// 固定 sleep によるレースコンディションが発生しない。
///
/// `setsid()` が EPERM 等で失敗した場合、または `spawn()` 自体が失敗した場合は
/// `Err` を返す。setsid() 失敗時は SIGTERM 保護が成立せず Zed のターミナル終了で
/// osascript が一緒に殺されて貼り付けが黙って失敗するため、サイレント化させず
/// 呼び出し側で fallback メッセージを出させる。
pub fn inject_keystroke_after_delay(initial_delay: Duration) -> std::io::Result<()> {
    spawn_injector_with_program("osascript", initial_delay)
}

/// `inject_keystroke_after_delay` の本体。program を差し替えられるよう分離している
/// のは、setsid + spawn の Result 経路を `osascript` (実機 Zed 必要) に依存せず
/// テストするため (`/usr/bin/true` 等で正常パスを確認する)。
fn spawn_injector_with_program(program: &str, initial_delay: Duration) -> std::io::Result<()> {
    let mut cmd = Command::new(program);
    for line in build_script_args(initial_delay) {
        cmd.arg("-e").arg(line);
    }

    // stderr をログファイルに記録する。Accessibility 拒否エラーの診断に使う。
    let stderr = open_stderr_log(&osascript_log_path());

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);

    // fork 後の子プロセスで setsid() を呼び、Zed の SIGTERM から切り離す。
    // pre_exec は fork 後・exec 前に子プロセスで実行される。
    // setsid() / errno 読み取り / Error::from_raw_os_error は async-signal-safe
    // （heap 割り当てなし）のためここで呼ぶのは安全。
    //
    // setsid() は呼び出し元がプロセスグループリーダー (PID == PGID) の場合 EPERM で
    // -1 を返す。戻り値を捨てると失敗を検知できず、Zed の SIGTERM が osascript に
    // 届いて貼り付けが黙って失敗する。
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    cmd.spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `initial_delay` の具体値がテスト結果に影響しないケース向けの共通呼び出し
    /// （delay 値そのものを検証する `delay_value_is_embedded_correctly` /
    /// `delay_zero_is_formatted` を除く全テストで使う）。
    fn script_args_fixture() -> Vec<String> {
        build_script_args(Duration::from_millis(100))
    }

    #[test]
    fn delay_value_is_embedded_correctly() {
        let args = build_script_args(Duration::from_millis(100));
        assert_eq!(args[0], "delay 0.100");
    }

    #[test]
    fn delay_zero_is_formatted() {
        let args = build_script_args(Duration::ZERO);
        assert_eq!(args[0], "delay 0.000");
    }

    #[test]
    fn max_attempts_is_40() {
        let args = script_args_fixture();
        assert!(
            args.iter().any(|s| s == "set maxAttempts to 40"),
            "maxAttempts 行が見つからない: {args:?}"
        );
    }

    #[test]
    fn fallback_notification_text_is_present() {
        let args = script_args_fixture();
        let has_notification = args
            .iter()
            .any(|s| s.contains("display notification") && s.contains("agent-history-pick ⚠"));
        assert!(
            has_notification,
            "フォールバック通知行が見つからない: {args:?}"
        );
    }

    #[test]
    fn accessibility_error_notification_is_present() {
        // Accessibility 権限拒否時の通知行が存在することを確認する。
        // この行が欠けると TCC 拒否がサイレントになり Issue #37 が再発する。
        let args = script_args_fixture();
        assert!(
            args.iter()
                .any(|s| s.contains("アクセシビリティ") && s.contains("display notification")),
            "Accessibility 権限エラー通知行が見つからない: {args:?}"
        );
    }

    #[test]
    fn try_on_error_block_wraps_keystroke() {
        // `try` が `keystroke` より前、`on error` が `keystroke` より後に存在することで
        // keystroke が try/on error ブロックに包まれていることを検証する。
        let args = script_args_fixture();
        let try_pos = args.iter().position(|s| s == "try");
        let keystroke_pos = args
            .iter()
            .position(|s| s.contains("keystroke \"r\" using command down"));
        let on_error_pos = args.iter().position(|s| s.starts_with("on error"));
        let end_try_pos = args.iter().position(|s| s == "end try");

        assert!(try_pos.is_some(), "`try` 行が見つからない");
        assert!(keystroke_pos.is_some(), "`keystroke` 行が見つからない");
        assert!(on_error_pos.is_some(), "`on error` 行が見つからない");
        assert!(end_try_pos.is_some(), "`end try` 行が見つからない");

        assert!(
            try_pos.unwrap() < keystroke_pos.unwrap(),
            "`try` が `keystroke` より後にある"
        );
        assert!(
            keystroke_pos.unwrap() < on_error_pos.unwrap(),
            "`on error` が `keystroke` より前にある"
        );
        assert!(
            on_error_pos.unwrap() < end_try_pos.unwrap(),
            "`end try` が `on error` より前にある"
        );
    }

    #[test]
    fn script_args_count_is_25() {
        let args = script_args_fixture();
        assert_eq!(
            args.len(),
            25,
            "スクリプト行数が想定と異なる: {}",
            args.len()
        );
    }

    #[test]
    fn zed_activate_line_is_present() {
        // Zed をフロントに持ち上げる行が欠けると activate されず、
        // ポーリングがタイムアウトして fallback 通知に流れる。
        let args = script_args_fixture();
        assert!(
            args.iter()
                .any(|s| s == "tell application \"Zed\" to activate"),
            "Zed activate 行が見つからない: {args:?}"
        );
    }

    #[test]
    fn cmd_r_keystroke_line_is_present() {
        // この行が terminal::Paste 発火本体。`"r"` → 別キー、`command` → `option` 等の
        // 改変で paste が無効化されるが、行数や delay の検証では捕まらない。
        let args = script_args_fixture();
        assert!(
            args.iter()
                .any(|s| s == "keystroke \"r\" using command down"),
            "cmd-r keystroke 行が見つからない: {args:?}"
        );
    }

    #[test]
    fn polling_delay_is_50ms() {
        // ポーリング間隔。maxAttempts(40) × 0.05s = 2s の総待機時間設計を支える。
        let args = script_args_fixture();
        assert!(
            args.iter().any(|s| s == "delay 0.05"),
            "ポーリング間隔 (delay 0.05) 行が見つからない: {args:?}"
        );
    }

    #[test]
    fn spawn_injector_with_existing_program_returns_ok() {
        // 正常パス回帰検出。`/usr/bin/true` は引数を無視して exit 0 するため
        // 副作用なく setsid + pre_exec + spawn の Result 経路を通せる。
        // この test が落ちる = inject_keystroke_after_delay の Err 化リファクタが
        // 通常呼び出しを壊した、と一発で分かる。
        let result = spawn_injector_with_program("/usr/bin/true", Duration::from_millis(0));
        assert!(result.is_ok(), "正常パスが Err を返した: {result:?}");
    }

    #[test]
    fn spawn_injector_with_missing_program_returns_err() {
        // spawn() 失敗が呼び出し側に伝搬することを担保。`?` を消してしまうと
        // この test が落ちる。
        let result = spawn_injector_with_program(
            "/nonexistent/binary/agent-history-pick-test",
            Duration::from_millis(0),
        );
        assert!(result.is_err(), "存在しない binary でも Ok を返した");
    }

    #[test]
    fn post_focus_settle_delay_is_300ms() {
        // フォーカス取得直後の settle 時間。これが消えると Zed の入力受付前に
        // keystroke が送られ paste が取りこぼされる。
        let args = script_args_fixture();
        assert!(
            args.iter().any(|s| s == "delay 0.3"),
            "フォーカス取得後 settle (delay 0.3) 行が見つからない: {args:?}"
        );
    }

    // symlink 拒否・ファイル権限の自己修復は secure_log.rs のテストでカバー済み
    // （open_stderr_log は secure_log::open_hardened のごく薄いラッパーであり、
    // spawn_injector_with_existing_program_returns_ok が実際に open_stderr_log
    // を経由するため、配線自体もそこで確認できている）。
}
