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

/// osascript を新しいセッションで起動し、Zed がフォーカスを取り戻した後に cmd-r を送る。
///
/// `initial_delay` はタスクターミナルが閉じ始めるのを待つ最小時間。
/// その後 AppleScript のポーリングで Zed が前面になるまで待機するため、
/// 固定 sleep によるレースコンディションが発生しない。
pub fn inject_keystroke_after_delay(initial_delay: Duration) {
    let delay_secs = initial_delay.as_secs_f64();
    let delay_line = format!("delay {delay_secs:.3}");

    let script_lines: &[&str] = &[
        delay_line.as_str(),
        "tell application \"Zed\" to activate",
        // Zed が実際に前面に来るまでポーリング（最大 2 秒 = 0.05s × 40）
        "set maxAttempts to 40",
        "set gotFocus to false",
        "repeat maxAttempts times",
        "delay 0.05",
        "tell application \"System Events\"",
        "if (name of first process whose frontmost is true) is \"Zed\" then",
        "set gotFocus to true",
        "exit repeat",
        "end if",
        "end tell",
        "end repeat",
        "if gotFocus then",
        "delay 0.3",
        "tell application \"System Events\"",
        "keystroke \"r\" using command down",
        "end tell",
        "else",
        "display notification \"Zed がフォーカスを取り戻せませんでした。クリップボードに内容はコピー済みです。手動で cmd-r を押してください。\" with title \"claude-history-pick ⚠\"",
        "end if",
    ];

    let mut cmd = Command::new("osascript");
    for line in script_lines {
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
