# agent-history-pick

Claude Code / Codex CLI のプロンプト履歴を fzf で統合検索し、選択したプロンプトを Zed 上の入力欄に自動貼り付けする Rust バイナリ。

## 対応履歴ソース

| ソース | デフォルトパス | override 環境変数 |
|---|---|---|
| Claude Code | `~/.claude/history.jsonl` | `CLAUDE_HISTORY_PATH` |
| Codex CLI | `$CODEX_HOME/history.jsonl`（`CODEX_HOME` 未設定時は `~/.codex/history.jsonl`） | `CODEX_HISTORY_PATH` / `CODEX_HOME` |

両ソースの履歴はタイムスタンプで統合・重複除去（新しい順）され、fzf 上に `[Claude]` / `[Codex]` の色分けラベル付きで表示される。

## 動作フロー

```
ctrl-; r
  → Zed タスク起動（tasks.json）
  → fzf で履歴を表示・選択
  → クリップボードにコピー（pbcopy）
  → setsid で独立プロセス（osascript）を起動
  → Zed がフォーカスを取り戻すまでポーリング（最大 2 秒）
  → cmd-r を送信 → terminal::Paste でクリップボードの内容を貼り付け
  → （AGENT_HISTORY_PICK_AUTO_ENTER 有効時のみ）Enter も送信
```

## 履歴の表示仕様

| 仕様 | 動作 |
|---|---|
| 表示順 | **新しい順**（最後に入力したプロンプトが先頭、両ソースを統合してソート） |
| 表示ラベル | `[Claude]`（シアン）/ `[Codex]`（マゼンタ）で出所を色分け表示 |
| 重複 | **自動除去**（同じテキストは1件のみ表示。最新の出現位置を優先） |
| スラッシュコマンド | 単独形（`/help` `/code-review` 等、`/` + 英数/ハイフン/アンダースコアのみ）は除外。引数や記号を伴うもの（`/loop 5m /foo` `/code-review --comment` `/foo:bar` 等）は採用 |
| 空エントリ | 除外 |
| ペースト | `[Pasted text #N ...]` は一覧では短縮表示のまま。選択して貼り付けるときに実ペースト内容へ展開される |

fuzzy 検索で絞り込みながら選択できる。複数行プロンプトは先頭行のみ候補として表示され、選択後に全文が貼り付けられる。

## モジュール構成

```
src/
├── main.rs       — エントリポイント・全体フロー
├── claude.rs     — ~/.claude/history.jsonl のパース（serde_json）
├── codex.rs      — Codex CLI の history.jsonl のパース
├── history.rs    — Prompt 共通表現・merge_sort_dedup によるソース統合
├── picker.rs     — fzf 起動・選択結果取得
├── clipboard.rs  — pbcopy でクリップボードにコピー
├── guard.rs      — PID ロックファイルによる単一インスタンス保証
├── injector.rs   — setsid + osascript でキーストローク注入
├── debug_log.rs  — 起動時の統合結果・選択結果を /tmp にデバッグ用途で記録
└── secure_log.rs — /tmp ログ open の symlink 攻撃対策・権限強制の共通処理
```

起動のたびに `/tmp/{uid}.agent-history-pick.debug.log` へ、統合結果の上位10件（`STARTUP`）と実際に選んだ内容（`SELECTED`）が追記される。所有者のみ読み書き可能（0600）、サイズが 1MB を超えると `.1` へ 1 世代だけ退避される。

## 依存

| ツール | 用途 | インストール |
|---|---|---|
| `fzf` (>= 0.20) | 対話型 fuzzy finder | `brew install fzf` |
| `pbcopy` | クリップボード書き込み | macOS 標準（不要） |
| `osascript` | キーストローク注入 | macOS 標準（不要） |
| `cargo` | ビルド | [rustup.rs](https://rustup.rs) |

fzf は `--with-nth` / `--delimiter` を使うため 0.20 以降が必要（`brew install fzf` の現行 0.44+ は要件を満たす）。

## セットアップ

```bash
# クローン & ビルド
git clone https://github.com/824ysuk/agent-history-pick
cd agent-history-pick
cargo build --release

# ~/.local/bin に配置
mkdir -p ~/.local/bin
ln -sf "$PWD/target/release/agent-history-pick" ~/.local/bin/agent-history-pick
```

## アップデート

PR マージ後や `git pull` 後は **`cargo build --release` を実行してバイナリを再ビルドする必要がある**。
シムリンクは `target/release/` を直接指しているため再作成不要。

```bash
cd ~/Projects/agent-history-pick
git pull origin main
cargo build --release
```

## 環境変数

| 変数 | デフォルト | 説明 |
|---|---|---|
| `CLAUDE_HISTORY_PATH` | `~/.claude/history.jsonl` | Claude Code 履歴ファイルのパス |
| `CODEX_HISTORY_PATH` | (未設定) | Codex CLI 履歴ファイルのパス（`CODEX_HOME` より優先） |
| `CODEX_HOME` | `~/.codex` | Codex CLI 自身のホームディレクトリ。`$CODEX_HOME/history.jsonl` を履歴として読む |
| `AGENT_HISTORY_PICK_AUTO_ENTER` | (未設定 = 無効) | 貼り付け（cmd-r）後に自動で Enter まで送信するかどうか。有効値: `1` / `true` / `yes` / `on`（大文字小文字・前後空白は無視）。それ以外の値は無効として扱う |

デフォルトで `AGENT_HISTORY_PICK_AUTO_ENTER` は無効。テンプレートを貼り付けて番号や引数の一部だけ編集してから送信する、という利用方法を壊さないため。

```bash
# 例: 別パスを使う
CLAUDE_HISTORY_PATH=/path/to/history.jsonl agent-history-pick
```

## Zed 設定

### tasks.json

`~/.config/zed/tasks.json` にタスクを追加する。

```json
[
  {
    "label": "Agent History Search",
    "command": "~/.local/bin/agent-history-pick",
    "use_new_terminal": true,
    "reveal": "always",
    "hide": "on_success"
  }
]
```

`ctrl-; r` の実経路ではこのタスクが本バイナリを起動するため、`AGENT_HISTORY_PICK_AUTO_ENTER` を常用したい場合はタスク側の `env` に指定する（対話シェルの環境変数はこのタスク実行には引き継がれない）。

```json
[
  {
    "label": "Agent History Search",
    "command": "~/.local/bin/agent-history-pick",
    "use_new_terminal": true,
    "reveal": "always",
    "hide": "on_success",
    "env": { "AGENT_HISTORY_PICK_AUTO_ENTER": "1" }
  }
]
```

### keymap.json

`~/.config/zed/keymap.json` の `Terminal` コンテキストに2つのバインディングを追加する。

```json
{
  "context": "Terminal",
  "bindings": {
    "ctrl-; r": ["task::Spawn", { "task_name": "Agent History Search" }],
    "cmd-r": "terminal::Paste"
  }
}
```

- `ctrl-; r` — fzf を起動してプロンプトを選択
- `cmd-r` — 選択後に osascript が自動送信するキー。`terminal::Paste` でクリップボード内容を入力欄に貼り付ける

## macOS Accessibility 権限

初回起動時に osascript のキーストローク注入に Accessibility 権限が必要。

**システム設定 → プライバシーとセキュリティ → アクセシビリティ** で Zed にチェックを入れる。
