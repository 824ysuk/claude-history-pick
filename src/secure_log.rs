//! `/tmp` 配下のログファイルを安全に open する共通処理。
//!
//! 責務: symlink 攻撃対策（`O_NOFOLLOW`）とファイル権限の強制（0600）のみ。
//! debug_log.rs（プロンプト履歴のデバッグログ）と injector.rs（osascript の
//! stderr ログ）が同一のセキュリティ方針を独立に実装すると、一方だけ変更されて
//! 対策がズレる（drift）リスクがあるため、方針そのものをここへ集約する。
//!
//! ## symlink 攻撃対策
//!
//! いずれのログパスも UID から決定的に導出される予測可能パスのため、
//! マルチユーザー環境では他ユーザーが事前に symlink を仕込む競合
//! （CWE-59 / TOCTOU）が理論上成立する。`open(2)` に `O_NOFOLLOW` を指定し、
//! symlink 経由のオープンを OS レベルで拒否する。
//!
//! ## ファイル権限
//!
//! ログにはプロンプト本文等の機密情報が含まれ得るため、所有者のみ読み書き
//! 可能（0600）に制限する。`mode()` は create と同一の `open(2)` 呼び出し内で
//! 適用されるため、作成直後に緩い権限で存在する window が生じない。ただし
//! umask に削られる方向にしか効かず、かつ既存ファイル（本対策より前に作成され
//! 緩い権限のまま残っているものを含む）には適用されないため、open 成功後に
//! `set_permissions` で明示的に強制する（自己修復）。

use std::fs::{File, OpenOptions, Permissions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// ログファイルに要求する権限（所有者のみ読み書き）。
const MODE: u32 = 0o600;

/// `path` を追記モードで安全に open する。
///
/// symlink 経由の open は拒否され `Err` を返す。権限強制（`set_permissions`）が
/// 失敗した場合も `Err` を返す（権限を保証できない状態で書き込みを続けるより、
/// 呼び出し側で書き込みを諦めさせる方が安全なため fail-closed にする）。
pub fn open_hardened(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .append(true)
        .create(true)
        .mode(MODE)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.set_permissions(Permissions::from_mode(MODE))?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn open_hardened_creates_file_with_owner_only_permissions() {
        let dir = tempfile::tempdir().expect("tempdir 作成に失敗");
        let path = dir.path().join("log");

        open_hardened(&path).expect("open に失敗");

        let mode = std::fs::metadata(&path)
            .expect("metadata 取得に失敗")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "新規作成ファイルの権限が 0600（所有者のみ）になっていない: {mode:o}"
        );
    }

    #[test]
    fn open_hardened_tightens_permissions_of_preexisting_world_readable_file() {
        let dir = tempfile::tempdir().expect("tempdir 作成に失敗");
        let path = dir.path().join("log");
        std::fs::write(&path, "pre-existing content from before this fix").expect("下準備に失敗");
        std::fs::set_permissions(&path, Permissions::from_mode(0o644))
            .expect("下準備の chmod に失敗");

        open_hardened(&path).expect("open に失敗");

        let mode = std::fs::metadata(&path)
            .expect("metadata 取得に失敗")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "本対策より前に作成された緩い権限のファイルが自己修復されていない: {mode:o}"
        );
    }

    /// symlink 経由の open が `O_NOFOLLOW` により拒否され、symlink の指す先の
    /// ファイルが書き換えられないことを確認する（CWE-59 対策の実効性テスト）。
    #[test]
    fn open_hardened_refuses_to_follow_symlink() {
        let dir = tempfile::tempdir().expect("tempdir 作成に失敗");
        let victim = dir.path().join("victim.txt");
        let log_path = dir.path().join("log");
        std::fs::write(&victim, "original victim content").expect("victim 作成に失敗");
        symlink(&victim, &log_path).expect("symlink 作成に失敗");

        let result = open_hardened(&log_path);

        assert!(
            result.is_err(),
            "symlink 経由の open が O_NOFOLLOW で拒否されなかった"
        );
        assert_eq!(
            std::fs::read_to_string(&victim).expect("victim 読み込みに失敗"),
            "original victim content",
            "symlink 経由で victim ファイルが書き換えられてしまった"
        );
    }
}
