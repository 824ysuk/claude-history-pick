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
use std::path::{Path, PathBuf};
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

fn read_pid(path: &Path) -> Option<libc::pid_t> {
    let mut file = fs::File::open(path).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    buf.trim().parse().ok()
}

fn write_pid(path: &Path) {
    if let Ok(mut file) = fs::File::create(path) {
        let _ = writeln!(file, "{}", std::process::id());
    }
}

/// PID が自バイナリと同一の実行ファイルかを確認する（PID 再利用対策）。
///
/// macOS では process name (comm) が 15 文字に切り詰められるため
/// `pgrep -x claude-history-pick`（19 文字）は常に空を返す。
/// `ps -o comm=` は argv[0] を返す。PATH 経由起動ではベア名（例: `claude-history-pick`）、
/// フルパス起動ではフルパスになるため、basename で比較する。
///
/// basename 一致で判定することで、バイナリを別名でコピーしても誤 kill しない。
/// hardcoded 文字列への依存も排除する。
fn is_our_process(pid: libc::pid_t) -> bool {
    let Ok(our_exe) = std::env::current_exe() else {
        return false;
    };
    let Some(exe_basename) = our_exe.file_name() else {
        return false;
    };
    let exe_basename = exe_basename.to_string_lossy();
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()
        .map(|out| {
            let comm = String::from_utf8_lossy(&out.stdout);
            let comm = comm.trim();
            // PATH 経由起動: comm はベア名。フルパス起動: comm はフルパス。両方を許容する。
            comm == exe_basename.as_ref() || comm.ends_with(&format!("/{exe_basename}"))
        })
        .unwrap_or(false)
}

/// テスト用: 任意パスへの read_pid / write_pid を公開する。
#[cfg(test)]
pub fn read_pid_from(path: &Path) -> Option<libc::pid_t> {
    read_pid(path)
}

#[cfg(test)]
pub fn write_pid_to(path: &Path) {
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
        let tmp = tempfile::NamedTempFile::new().unwrap();
        write_pid_to(tmp.path());
        let read = read_pid_from(tmp.path()).expect("PID が読めない");
        assert_eq!(read, std::process::id() as libc::pid_t);
        // tmp が Drop するとファイルは自動削除される
    }

    #[test]
    fn read_pid_returns_none_for_missing_file() {
        let path = PathBuf::from("/tmp/guard-test-nonexistent-99999.lock");
        assert!(read_pid_from(&path).is_none());
    }

    #[test]
    fn read_pid_returns_none_for_malformed_content() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        writeln!(tmp, "not-a-pid").unwrap();
        assert!(read_pid_from(tmp.path()).is_none());
        // tmp が Drop するとファイルは自動削除される
    }

    #[test]
    fn ps_comm_matches_exe_basename_or_fullpath() {
        // is_our_process の動作基盤となる前提を直接検証する:
        // ps -o comm= は argv[0] を返す。フルパス起動ならフルパス、
        // PATH 経由（ベア名）起動ならベア名になるため、basename との一致を確認する。
        let my_pid = std::process::id() as libc::pid_t;
        let exe = std::env::current_exe().unwrap();
        let exe_basename = exe.file_name().unwrap().to_string_lossy();
        let output = std::process::Command::new("ps")
            .args(["-p", &my_pid.to_string(), "-o", "comm="])
            .output()
            .unwrap();
        let comm = String::from_utf8_lossy(&output.stdout);
        let comm = comm.trim();
        assert!(
            comm == exe_basename.as_ref() || comm.ends_with(&format!("/{exe_basename}")),
            "ps -o comm= はベア名かフルパスを返すはず (got: {comm:?}, basename: {exe_basename:?})"
        );
    }

    #[test]
    fn is_our_process_recognizes_self() {
        // basename 一致で判定するため、
        // ディレクトリ名に依存せず自プロセスを識別できる。
        let my_pid = std::process::id() as libc::pid_t;
        assert!(is_our_process_pub(my_pid), "自プロセスを識別できるはず");
    }

    #[test]
    fn is_our_process_returns_false_for_dead_pid() {
        // 存在しない PID（大きな値）は false
        assert!(!is_our_process_pub(999_999));
    }

    // acquire/release は実際のロックファイルを操作するため直列化する。
    // 並列実行すると acquire が相手の lock を evict してしまう。
    static LOCK_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn acquire_writes_current_pid() {
        let _guard = LOCK_MUTEX.lock().unwrap();
        let path = lock_path();
        let _ = std::fs::remove_file(&path); // 残留ロックを除去してクリーンな状態にする

        acquire();

        let pid = read_pid_from(&path).expect("acquire がロックファイルを作成していない");
        assert_eq!(pid, std::process::id() as libc::pid_t);

        let _ = std::fs::remove_file(&path); // 後始末
    }

    #[test]
    fn release_removes_lock_file() {
        let _guard = LOCK_MUTEX.lock().unwrap();
        let path = lock_path();

        acquire();
        assert!(path.exists(), "前提: acquire 後にロックファイルが存在する");

        release();
        assert!(!path.exists(), "release 後にロックファイルが残っている");
    }

    #[test]
    fn acquire_succeeds_when_lock_contains_malformed_pid() {
        // ロックファイルが壊れた内容で残っていても、acquire は何事もなく
        // 自 PID で上書きする (read_pid が None を返す経路の結合検証)。
        let _guard = LOCK_MUTEX.lock().unwrap();
        let path = lock_path();
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "not-a-pid\n").expect("malformed lock を書けない");

        acquire();

        let pid = read_pid_from(&path).expect("acquire 後に PID が読めない");
        assert_eq!(
            pid,
            std::process::id() as libc::pid_t,
            "malformed lock を上書きできていない"
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn acquire_succeeds_when_lock_is_empty() {
        // 0 byte のロックファイル (kill -9 / ディスク full 等で発生しうる) でも
        // acquire は自 PID で上書きする。
        let _guard = LOCK_MUTEX.lock().unwrap();
        let path = lock_path();
        let _ = std::fs::remove_file(&path);
        std::fs::File::create(&path).expect("空ファイルを作れない");

        acquire();

        let pid = read_pid_from(&path).expect("acquire 後に PID が読めない");
        assert_eq!(pid, std::process::id() as libc::pid_t);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn acquire_overwrites_stale_dead_pid() {
        // 死んでいる PID が書かれている場合: is_our_process が false を返して
        // evict を skip し、そのまま write_pid に進んで自 PID で上書きする。
        let _guard = LOCK_MUTEX.lock().unwrap();
        let path = lock_path();
        let _ = std::fs::remove_file(&path);
        std::fs::write(&path, "999999\n").expect("stale PID を書けない");

        acquire();

        let pid = read_pid_from(&path).expect("acquire 後に PID が読めない");
        assert_eq!(
            pid,
            std::process::id() as libc::pid_t,
            "stale dead PID を上書きできていない"
        );

        let _ = std::fs::remove_file(&path);
    }
}
