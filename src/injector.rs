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

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::Duration;

/// osascript に渡す `-e` 引数のリストを構築する（純粋関数）。
///
/// 各要素が `osascript -e <element>` の 1 行に対応する。
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
        "tell application \"System Events\"".to_string(),
        "keystroke \"r\" using command down".to_string(),
        "end tell".to_string(),
        "else".to_string(),
        "display notification \"Zed がフォーカスを取り戻せませんでした。クリップボードに内容はコピー済みです。手動で cmd-r を押してください。\" with title \"claude-history-pick ⚠\"".to_string(),
        "end if".to_string(),
    ]
}

/// osascript を新しいセッションで起動し、Zed がフォーカスを取り戻した後に cmd-r を送る。
///
/// `initial_delay` はタスクターミナルが閉じ始めるのを待つ最小時間。
/// その後 AppleScript のポーリングで Zed が前面になるまで待機するため、
/// 固定 sleep によるレースコンディションが発生しない。
pub fn inject_keystroke_after_delay(initial_delay: Duration) {
    let mut cmd = Command::new("osascript");
    for line in build_script_args(initial_delay) {
        cmd.arg("-e").arg(line);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    // fork 後の子プロセスで setsid() を呼び、Zed の SIGTERM から切り離す。
    // pre_exec は fork 後・exec 前に子プロセスで実行される。
    // setsid() は POSIX async-signal-safe 関数のためここで呼ぶのは安全。
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    cmd.spawn().ok();
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let args = build_script_args(Duration::from_millis(500));
        assert!(
            args.iter().any(|s| s == "set maxAttempts to 40"),
            "maxAttempts 行が見つからない: {args:?}"
        );
    }

    #[test]
    fn fallback_notification_text_is_present() {
        let args = build_script_args(Duration::from_millis(500));
        let has_notification = args
            .iter()
            .any(|s| s.contains("display notification") && s.contains("claude-history-pick ⚠"));
        assert!(
            has_notification,
            "フォールバック通知行が見つからない: {args:?}"
        );
    }

    #[test]
    fn script_args_count_is_21() {
        let args = build_script_args(Duration::from_millis(500));
        assert_eq!(
            args.len(),
            21,
            "スクリプト行数が想定と異なる: {}",
            args.len()
        );
    }

    #[test]
    fn zed_activate_line_is_present() {
        // Zed をフロントに持ち上げる行が欠けると activate されず、
        // ポーリングがタイムアウトして fallback 通知に流れる。
        let args = build_script_args(Duration::from_millis(100));
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
        let args = build_script_args(Duration::from_millis(100));
        assert!(
            args.iter()
                .any(|s| s == "keystroke \"r\" using command down"),
            "cmd-r keystroke 行が見つからない: {args:?}"
        );
    }

    #[test]
    fn polling_delay_is_50ms() {
        // ポーリング間隔。maxAttempts(40) × 0.05s = 2s の総待機時間設計を支える。
        let args = build_script_args(Duration::from_millis(100));
        assert!(
            args.iter().any(|s| s == "delay 0.05"),
            "ポーリング間隔 (delay 0.05) 行が見つからない: {args:?}"
        );
    }

    #[test]
    fn post_focus_settle_delay_is_300ms() {
        // フォーカス取得直後の settle 時間。これが消えると Zed の入力受付前に
        // keystroke が送られ paste が取りこぼされる。
        let args = build_script_args(Duration::from_millis(100));
        assert!(
            args.iter().any(|s| s == "delay 0.3"),
            "フォーカス取得後 settle (delay 0.3) 行が見つからない: {args:?}"
        );
    }
}
