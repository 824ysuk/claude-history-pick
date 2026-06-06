//! Zed へのキーストローク注入層（double-fork + osascript）。
//!
//! 責務: Zed のプロセスグループから切り離され、タスクターミナル終了後に
//! cmd-r（terminal::Paste）を発火させる孤立プロセスの生成のみ。
//! クリップボードや履歴パースは扱わない。
//!
//! ## なぜ double-fork が必要か
//!
//! Zed は `hide: on_success` でタスクターミナルタブを閉じるとき、
//! そのターミナルが属するプロセスグループ全体に SIGTERM を送る。
//!
//! `os.setsid()` だけでは「新しいセッションを作るが、自プロセスは
//! 新セッションのプロセスグループリーダー」になるため、Zed が
//! グループリーダーを kill する設計だと依然として届く場合がある。
//!
//! double-fork（fork → setsid → fork）によって孫プロセスを作ると、
//! 孫は「新セッションのプロセスグループリーダーでない」孤立プロセスになり
//! SIGTERM が届かなくなる。
//!
//! ## 孫での exec
//!
//! fork 後の子プロセスで Rust ランタイム（スレッド等）が複数の状態を持つと
//! 安全でない動作が起きうる。孫では sleep/osascript を sh 経由で exec し
//! Rust ランタイムを置き換えることで POSIX async-signal-safe 規約を守る。

use std::ffi::CString;
use std::time::Duration;

/// double-fork でデーモン化し、`delay` 後に Zed へ cmd-r を送る。
///
/// この関数は呼び出し元の親プロセスから戻るが、孫プロセスは
/// バックグラウンドで `delay` ミリ秒後に osascript を起動する。
pub fn inject_keystroke_after_delay(delay: Duration) {
    let delay_secs = delay.as_secs_f64();

    // osascript 2 コマンドを sh -c で繋いだ文字列
    // 遅延後に (1) Zed をアクティブ化、(2) cmd-r をキーストローク送信
    let script = format!(
        "sleep {delay_secs:.3} \
         && osascript -e 'tell application \"Zed\" to activate' \
         && sleep 0.1 \
         && osascript -e 'tell application \"System Events\" \
                          to keystroke \"r\" using command down'"
    );

    // CString への変換（exec に必要な null 終端バイト列）
    let c_sh = match CString::new("sh") {
        Ok(s) => s,
        Err(_) => return,
    };
    let c_dash_c = match CString::new("-c") {
        Ok(s) => s,
        Err(_) => return,
    };
    let c_script = match CString::new(script) {
        Ok(s) => s,
        Err(_) => return,
    };

    unsafe {
        // ── 第 1 fork ──────────────────────────────────────────────
        // 子が setsid() で新しいセッションを作る。
        // 親（＝このプロセス）はすぐ wait して戻る。
        match libc::fork() {
            -1 => return, // fork 失敗: 静かに諦める（fzf 選択は成功済み）
            0 => {
                // ── 子プロセス ────────────────────────────────────
                // 新セッションを作成し、制御端末から切り離す。
                // ただしこの時点で子はセッションリーダーのため
                // 後述の第 2 fork が必要。
                libc::setsid();

                // ── 第 2 fork ──────────────────────────────────────
                // 孫を作り、子は即 exit する。
                // 孫は「セッションリーダーでないプロセス」になるため
                // Zed の SIGTERM から完全に切り離される。
                match libc::fork() {
                    -1 => libc::_exit(1),
                    0 => {
                        // ── 孫プロセス ────────────────────────────
                        // exec で Rust ランタイムを sh に置き換える。
                        // これにより fork 後の async-signal-safe 制約を回避する。
                        let argv: &[*const libc::c_char] = &[
                            c_sh.as_ptr(),
                            c_dash_c.as_ptr(),
                            c_script.as_ptr(),
                            std::ptr::null(),
                        ];
                        libc::execvp(c_sh.as_ptr(), argv.as_ptr());
                        // execvp が失敗した場合のみここに到達
                        libc::_exit(1);
                    }
                    _ => libc::_exit(0), // 第 2 fork の子（孫の親）: すぐ終了
                }
            }
            _ => {
                // ── 第 1 fork の親 ────────────────────────────────
                // 子の終了を wait してゾンビを回収してから呼び出し元へ戻る。
                libc::wait(std::ptr::null_mut());
            }
        }
    }
}
