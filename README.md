# claude-history-pick

`~/.claude/history.jsonl` を fzf で検索し、選択したプロンプトを Zed 上の Claude Code 入力欄に自動貼り付けする Rust バイナリ。

## 動作フロー

```
ctrl-; r
  → Zed タスク起動（tasks.json）
  → fzf で履歴を表示・選択
  → クリップボードにコピー（pbcopy）
  → setsid で独立プロセス（osascript）を起動
  → Zed がフォーカスを取り戻すまでポーリング（最大 2 秒）
  → cmd-r を送信 → terminal::Paste でクリップボードの内容を貼り付け
```

## 履歴の表示仕様

| 仕様 | 動作 |
|---|---|
| 表示順 | **新しい順**（最後に入力したプロンプトが先頭） |
| 重複 | **自動除去**（同じテキストは1件のみ表示。最新の出現位置を優先） |
| スラッシュコマンド | 引数なし（`/help` `/clear` 等）は除外。引数付き（`/loop 5m /foo` `/code-review --comment` 等）は採用 |
| 空エントリ | 除外 |

fuzzy 検索で絞り込みながら選択できる。複数行プロンプトは先頭行のみ候補として表示され、選択後に全文が貼り付けられる。

## モジュール構成

```
src/
├── main.rs       — エントリポイント・全体フロー
├── history.rs    — ~/.claude/history.jsonl のパース（serde_json）
├── picker.rs     — fzf 起動・選択結果取得
├── clipboard.rs  — pbcopy でクリップボードにコピー
├── guard.rs      — PID ロックファイルによる単一インスタンス保証
└── injector.rs   — setsid + osascript でキーストローク注入
```

## 依存

| ツール | 用途 | インストール |
|---|---|---|
| `fzf` | 対話型 fuzzy finder | `brew install fzf` |
| `pbcopy` | クリップボード書き込み | macOS 標準（不要） |
| `osascript` | キーストローク注入 | macOS 標準（不要） |
| `cargo` | ビルド | [rustup.rs](https://rustup.rs) |

## セットアップ

```bash
# クローン & ビルド
git clone https://github.com/824ysuk/claude-history-pick
cd claude-history-pick
cargo build --release

# ~/.local/bin に配置
mkdir -p ~/.local/bin
ln -sf "$PWD/target/release/claude-history-pick" ~/.local/bin/claude-history-pick
```

## 環境変数

| 変数 | デフォルト | 説明 |
|---|---|---|
| `CLAUDE_HISTORY_PATH` | `~/.claude/history.jsonl` | 履歴ファイルのパス |

```bash
# 例: 別パスを使う
CLAUDE_HISTORY_PATH=/path/to/history.jsonl claude-history-pick
```

## Zed 設定

### tasks.json

`~/.config/zed/tasks.json` にタスクを追加する。

```json
[
  {
    "label": "Claude History Search",
    "command": "~/.local/bin/claude-history-pick",
    "use_new_terminal": true,
    "reveal": "always",
    "hide": "on_success"
  }
]
```

### keymap.json

`~/.config/zed/keymap.json` の `Terminal` コンテキストに2つのバインディングを追加する。

```json
{
  "context": "Terminal",
  "bindings": {
    "ctrl-; r": ["task::Spawn", { "task_name": "Claude History Search" }],
    "cmd-r": "terminal::Paste"
  }
}
```

- `ctrl-; r` — fzf を起動してプロンプトを選択
- `cmd-r` — 選択後に osascript が自動送信するキー。`terminal::Paste` でクリップボード内容を入力欄に貼り付ける

## macOS Accessibility 権限

初回起動時に osascript のキーストローク注入に Accessibility 権限が必要。

**システム設定 → プライバシーとセキュリティ → アクセシビリティ** で Zed にチェックを入れる。
