//! 単一インスタンス保証（PID ロックファイル方式）。
//!
//! 責務: 先行プロセスの検出・排除と自 PID の記録のみ。
//! 履歴・fzf・クリップボード・キーストロークは扱わない。
//!
//! ## なぜ PID ロックファイルか
//!
//! `pkill -x fzf` は agent-history-pick 以外の fzf を巻き込む。
//! ロックファイルに先行インスタンスの PID を記録することで、
//! その子（fzf）だけを `pkill -P <pid>` で正確に終了させられる。
//!
//! ## ロックファイルの配置
//!
//! /tmp/<uid>.agent-history-pick.lock を使う。
//! UID を含めることでマルチユーザー環境での衝突を防ぎ、
//! /tmp の性質上 OS 再起動で自動消滅するため古いロックが残らない。
//!
//! ## PID 再利用対策
//!
//! ロックファイルの PID が生きていても、プロセス名が
//! agent-history-pick でなければ kill しない。
//!
//! ## アトミックなロック取得
//!
//! O_CREAT|O_EXCL（create_new=true）で書き込む。
//! 競合する 2 インスタンスが同時に「ロックなし」と判定して並走する
//! TOCTOU を防ぐ。

use nix::errno::Errno;
use nix::sys::signal::{kill, Signal};
use nix::unistd::{getuid, Pid};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn lock_path() -> PathBuf {
    PathBuf::from(format!("/tmp/{}.agent-history-pick.lock", getuid()))
}

/// 単一インスタンス権を取得する。
///
/// O_CREAT|O_EXCL でアトミックにロックファイルを生成し TOCTOU を防ぐ。
/// 既存ロックがある場合は先行プロセスを排除してリトライする。
pub fn acquire() {
    let path = lock_path();
    const MAX_RETRIES: u32 = 3;

    for _ in 0..MAX_RETRIES {
        match try_write_pid_exclusive(&path) {
            Ok(()) => return,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                if let Some(old_pid) = read_pid(&path) {
                    if is_our_process(old_pid) {
                        evict(old_pid);
                    } else {
                        // 死んでいる PID が残っているだけなので上書きできる
                        let _ = fs::remove_file(&path);
                    }
                } else {
                    // 壊れたロックファイル — 削除して再試行
                    let _ = fs::remove_file(&path);
                }
            }
            Err(_) => {
                // AlreadyExists 以外の IO エラーは回復不能 — fallback で通常書き込み
                write_pid(&path);
                return;
            }
        }
    }

    // MAX_RETRIES 回の競合後は通常書き込みで続行
    write_pid(&path);
}

/// ロックファイルを削除する。
///
/// 正常終了・キャンセル・エラー終了すべての exit 前に呼ぶ。
/// 削除失敗は次回起動時の evict() で自動回復するため無視する。
pub fn release() {
    let _ = fs::remove_file(lock_path());
}

/// O_CREAT|O_EXCL でアトミックに自 PID を書き込む。
///
/// ファイルが既に存在する場合は `ErrorKind::AlreadyExists` を返す。
fn try_write_pid_exclusive(path: &Path) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    writeln!(file, "{}", std::process::id())
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
/// `pgrep -x agent-history-pick`（18 文字）は常に空を返す。
/// `ps -o comm=` は argv[0] を返す。PATH 経由起動ではベア名（例: `agent-history-pick`）、
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
pub fn try_write_pid_exclusive_pub(path: &Path) -> io::Result<()> {
    try_write_pid_exclusive(path)
}

#[cfg(test)]
pub fn is_our_process_pub(pid: libc::pid_t) -> bool {
    is_our_process(pid)
}

/// 先行インスタンスとその子プロセス（fzf）を終了させ、通知を出す。
fn evict(old_pid: libc::pid_t) {
    // fzf は agent-history-pick の子なので先に終了させる
    std::process::Command::new("pkill")
        .args(["-P", &old_pid.to_string()])
        .status()
        .ok();

    kill(Pid::from_raw(old_pid), Signal::SIGTERM).ok();

    // SIGTERM 後に ncurses 終了処理が完了するまでポーリングで待つ。
    // 固定 sleep だと fzf の ncurses クリーンアップが 50ms を超えた場合に
    // 新旧 2 インスタンスがクリップボードを同時操作して二重ペーストが発生する。
    // 500ms = 実測クリーンアップ上限（~50ms）の 10× 安全余裕。設計値（環境依存でない）。
    const SIGTERM_WAIT_TIMEOUT: Duration = Duration::from_millis(500);
    if !wait_for_death(old_pid, SIGTERM_WAIT_TIMEOUT) {
        kill(Pid::from_raw(old_pid), Signal::SIGKILL).ok();
    }

    // 何が邪魔していたかをユーザーに通知する。
    // ターミナルは hide: on_success で消えるため macOS 通知を使う。
    std::process::Command::new("osascript")
        .args([
            "-e",
            &format!(
                "display notification \
                 \"残留プロセス (PID {old_pid}) を終了しました。続けて操作できます。\" \
                 with title \"agent-history-pick\""
            ),
        ])
        .status()
        .ok();
}

/// プロセスの死亡をポーリングで確認する。
///
/// `kill(pid, 0)` は ESRCH でプロセス消滅を検知する（シグナルを送らない）。
/// `timeout` 以内に消滅すれば true を返す。
/// ポーリング間隔 20ms は CPU を抑えつつ死亡検知遅延を最小化する設計値。
fn wait_for_death(pid: libc::pid_t, timeout: Duration) -> bool {
    // 20ms × 最大 25 回 = 500ms で SIGTERM_WAIT_TIMEOUT と対応する設計値。
    const POLL_INTERVAL: Duration = Duration::from_millis(20);
    let start = Instant::now();
    while start.elapsed() < timeout {
        match kill(Pid::from_raw(pid), None) {
            Err(Errno::ESRCH) => return true, // プロセスが消滅
            _ => std::thread::sleep(POLL_INTERVAL),
        }
    }
    false
}

#[cfg(test)]
pub fn wait_for_death_pub(pid: libc::pid_t, timeout: Duration) -> bool {
    wait_for_death(pid, timeout)
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
    fn try_write_pid_exclusive_fails_when_file_exists() {
        // O_CREAT|O_EXCL: 既存ファイルに対して AlreadyExists を返すことを検証する。
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let err = try_write_pid_exclusive_pub(tmp.path()).expect_err("既存ファイルで Ok を返した");
        assert_eq!(
            err.kind(),
            io::ErrorKind::AlreadyExists,
            "AlreadyExists 以外のエラーを返した: {err}"
        );
    }

    #[test]
    fn try_write_pid_exclusive_succeeds_on_new_file() {
        // 存在しないパスに対して Ok を返し、自 PID が書き込まれることを検証する。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.lock");
        try_write_pid_exclusive_pub(&path).expect("新規ファイルで失敗した");
        let pid = read_pid_from(&path).expect("PID が読めない");
        assert_eq!(pid, std::process::id() as libc::pid_t);
    }

    #[test]
    fn wait_for_death_returns_true_for_dead_pid() {
        // すでに死んでいる PID に対して即 true を返すことを検証する。
        // 999_999 は macOS では通常存在しない PID。
        let dead_pid = 999_999_i32;
        assert!(
            wait_for_death_pub(dead_pid, Duration::from_millis(100)),
            "死んでいる PID で false を返した"
        );
    }

    #[test]
    fn wait_for_death_returns_false_on_timeout_for_live_process() {
        // 生きているプロセスに対して timeout まで待って false を返すことを検証する。
        // sleep コマンドを起動して即 reap する（zombie にしない）。
        let child = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .expect("sleep を起動できない");
        let pid = child.id() as libc::pid_t;
        // 短いタイムアウトで false が返ること（生きているため）
        let result = wait_for_death_pub(pid, Duration::from_millis(60));
        // 後始末: SIGKILL して reap
        kill(Pid::from_raw(pid), Signal::SIGKILL).ok();
        drop(child); // wait せず drop（zombie は OS が回収）
        assert!(!result, "生きているプロセスで true を返した");
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
