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
    let mut child = Command::new("pbcopy")
        .stdin(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
        // stdin drop → pbcopy に EOF が届きコピー完了
    }

    child.wait()?;
    Ok(())
}
