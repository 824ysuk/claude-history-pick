//! 単一インスタンス保証（PID ロックファイル方式）。
//!
//! 責務: 先行プロセスの検出・排除と自 PID の記録のみ。
//! 履歴・fzf・クリップボード・キーストロークは扱わない。
//!
//! ## なぜ PID ロックファイルか
//!
//! `pkill -x fzf` は claude-history-pick 以外の fzf を巻き込む。
//! ロックファイルに先行インスタンスの PID を記録することで、
//! その子（fzf）だけを `pkill -P <pid>` で正確に終了させられる。
//!
//! ## ロックファイルの配置
//!
//! /tmp/<uid>.claude-history-pick.lock を使う。
//! UID を含めることでマルチユーザー環境での衝突を防ぎ、
//! /tmp の性質上 OS 再起動で自動消滅するため古いロックが残らない。
//!
//! ## PID 再利用対策
//!
//! ロックファイルの PID が生きていても、プロセス名が
//! claude-history-pick でなければ kill しない。

use nix::sys::signal::{kill, Signal};
use nix::unistd::{getuid, Pid};
use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

fn lock_path() -> PathBuf {
    PathBuf::from(format!("/tmp/{}.claude-history-pick.lock", getuid()))
}

/// 単一インスタンス権を取得する。
///
/// ロックファイルに記録された先行プロセスが生きていれば
/// 子プロセス（fzf）ごと終了させてから自 PID を書く。
pub fn acquire() {
    let path = lock_path();

    if let Some(old_pid) = read_pid(&path) {
        if is_our_process(old_pid) {
            evict(old_pid);
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    write_pid(&path);
}

/// ロックファイルを削除する。
///
/// 正常終了・キャンセル・エラー終了すべての exit 前に呼ぶ。
/// 削除失敗は次回起動時の evict() で自動回復するため無視する。
pub fn release() {
    let _ = fs::remove_file(lock_path());
}

fn read_pid(path: &PathBuf) -> Option<libc::pid_t> {
    let mut file = fs::File::open(path).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    buf.trim().parse().ok()
}

fn write_pid(path: &PathBuf) {
    if let Ok(mut file) = fs::File::create(path) {
        let _ = writeln!(file, "{}", std::process::id());
    }
}

/// PID が claude-history-pick プロセスかを確認する（PID 再利用対策）。
///
/// macOS では process name (comm) が 15 文字に切り詰められるため
/// `pgrep -x claude-history-pick`（19 文字）は常に空を返す。
/// `ps -o comm=` は argv[0]（フルパス）を返すため切り詰めが起きない。
fn is_our_process(pid: libc::pid_t) -> bool {
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains("claude-history-pick"))
        .unwrap_or(false)
}

/// テスト用: 任意パスへの read_pid / write_pid を公開する。
#[cfg(test)]
pub fn read_pid_from(path: &PathBuf) -> Option<libc::pid_t> {
    read_pid(path)
}

#[cfg(test)]
pub fn write_pid_to(path: &PathBuf) {
    write_pid(path)
}

#[cfg(test)]
pub fn is_our_process_pub(pid: libc::pid_t) -> bool {
    is_our_process(pid)
}

/// 先行インスタンスとその子プロセス（fzf）を終了させ、通知を出す。
fn evict(old_pid: libc::pid_t) {
    // fzf は claude-history-pick の子なので先に終了させる
    std::process::Command::new("pkill")
        .args(["-P", &old_pid.to_string()])
        .status()
        .ok();

    kill(Pid::from_raw(old_pid), Signal::SIGTERM).ok();

    // 何が邪魔していたかをユーザーに通知する。
    // ターミナルは hide: on_success で消えるため macOS 通知を使う。
    std::process::Command::new("osascript")
        .args([
            "-e",
            &format!(
                "display notification \
                 \"残留プロセス (PID {old_pid}) を終了しました。続けて操作できます。\" \
                 with title \"claude-history-pick\""
            ),
        ])
        .status()
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn write_and_read_pid_roundtrip() {
        let path = PathBuf::from(format!("/tmp/guard-test-{}.lock", std::process::id()));
        write_pid_to(&path);
        let read = read_pid_from(&path).expect("PID が読めない");
        assert_eq!(read, std::process::id() as libc::pid_t);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn read_pid_returns_none_for_missing_file() {
        let path = PathBuf::from("/tmp/guard-test-nonexistent-99999.lock");
        assert!(read_pid_from(&path).is_none());
    }

    #[test]
    fn read_pid_returns_none_for_malformed_content() {
        let path = PathBuf::from(format!(
            "/tmp/guard-test-malformed-{}.lock",
            std::process::id()
        ));
        let mut f = std::fs::File::create(&path).unwrap();
        writeln!(f, "not-a-pid").unwrap();
        assert!(read_pid_from(&path).is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn is_our_process_recognizes_self() {
        // cargo test のバイナリは claude-history-pick 本体なので true になる。
        // これは ps -o comm= が argv[0] フルパスを返すことの確認でもある。
        let my_pid = std::process::id() as libc::pid_t;
        assert!(
            is_our_process_pub(my_pid),
            "テストバイナリ自体が claude-history-pick のはず"
        );
    }

    #[test]
    fn is_our_process_returns_false_for_dead_pid() {
        // 存在しない PID（大きな値）は false
        assert!(!is_our_process_pub(999_999));
    }
}
