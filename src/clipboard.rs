//! macOS クリップボードへのテキスト書き込み層。
//!
//! 責務: pbcopy を通じてクリップボードにテキストをセットするのみ。
//! pbcopy は macOS 標準付属のため追加インストール不要。

use std::io::Write;
use std::process::{Command, Stdio};

/// `text` を macOS クリップボードにコピーする。
///
/// pbcopy の stdin にテキストを書き込む。
/// fzf 選択結果を Zed の terminal::Paste で貼り付けるための中継地点。
pub fn copy_to_clipboard(text: &str) -> std::io::Result<()> {
    let mut child = Command::new("pbcopy").stdin(Stdio::piped()).spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
        // stdin drop → pbcopy に EOF が届きコピー完了
    }

    child.wait()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // pbcopy/pbpaste はシステムクリップボードを直接操作するため、
    // 並列実行すると相互に上書きして誤検知が起きる。
    static CLIPBOARD_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn pbpaste() -> String {
        let out = Command::new("pbpaste").output().expect("pbpaste failed");
        String::from_utf8(out.stdout).unwrap_or_default()
    }

    #[test]
    fn copy_to_clipboard_writes_text() {
        let _guard = CLIPBOARD_MUTEX.lock().unwrap();
        copy_to_clipboard("claude-history-pick test").expect("pbcopy failed");
        assert_eq!(pbpaste(), "claude-history-pick test");
    }

    #[test]
    fn copy_to_clipboard_preserves_unicode() {
        let _guard = CLIPBOARD_MUTEX.lock().unwrap();
        copy_to_clipboard("テスト 🦀").expect("pbcopy failed");
        assert_eq!(pbpaste(), "テスト 🦀");
    }

    #[test]
    fn copy_to_clipboard_preserves_newlines() {
        let _guard = CLIPBOARD_MUTEX.lock().unwrap();
        copy_to_clipboard("line1\nline2").expect("pbcopy failed");
        assert_eq!(pbpaste(), "line1\nline2");
    }
}
