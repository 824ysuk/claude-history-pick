//! `/tmp` 配下の UID スコープ付きパスを組み立てる共通処理。
//!
//! 責務: パス文字列の組み立てのみ。ファイル I/O は扱わない
//! （open は secure_log.rs、ロック取得のアトミック性は guard.rs が個別に担う）。
//! debug_log.rs（デバッグログ）・injector.rs（osascript stderr ログ）・
//! guard.rs（PID ロック）が `/tmp/{uid}.agent-history-pick.<suffix>` という
//! 同一の命名規則を独立実装していたため、規則そのものをここへ集約する。
//! 3 箇所とも同じ規則である以上、将来アプリ名や配置規則を変える際に
//! 1 箇所の変更で済み、一部だけ更新し忘れる不整合を防げる。

use std::path::PathBuf;

/// `/tmp/{uid}.agent-history-pick.<suffix>` 形式のパスを組み立てる。
///
/// UID を含めることでマルチユーザー環境での衝突を防ぐ。
pub fn uid_scoped_tmp_path(suffix: &str) -> PathBuf {
    let uid = nix::unistd::getuid();
    PathBuf::from(format!("/tmp/{uid}.agent-history-pick.{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_scoped_tmp_path_embeds_suffix() {
        let path = uid_scoped_tmp_path("example.log");
        let path_str = path.to_string_lossy();
        assert!(
            path_str.starts_with("/tmp/"),
            "パスが /tmp/ で始まっていない: {path_str}"
        );
        assert!(
            path_str.ends_with(".agent-history-pick.example.log"),
            "suffix が末尾に正しく埋め込まれていない: {path_str}"
        );
    }

    #[test]
    fn uid_scoped_tmp_path_embeds_real_uid() {
        let uid = nix::unistd::getuid();
        let path = uid_scoped_tmp_path("x");
        assert_eq!(
            path,
            PathBuf::from(format!("/tmp/{uid}.agent-history-pick.x")),
            "実際の getuid() の値がパスに反映されていない"
        );
    }

    #[test]
    fn uid_scoped_tmp_path_different_suffixes_produce_different_paths() {
        let debug_path = uid_scoped_tmp_path("debug.log");
        let osascript_path = uid_scoped_tmp_path("osascript.log");
        let lock_path = uid_scoped_tmp_path("lock");
        assert_ne!(debug_path, osascript_path);
        assert_ne!(debug_path, lock_path);
        assert_ne!(osascript_path, lock_path);
    }
}
