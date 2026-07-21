# PatchArena

[![CI](https://github.com/lasder-ca/PatchArena/actions/workflows/ci.yml/badge.svg)](https://github.com/lasder-ca/PatchArena/actions/workflows/ci.yml)

[English](README.md) | **日本語**

PatchArenaは、実際のGitリポジトリ上でAIコーディングエージェントを繰り返し実行し、その結果を比較するベンチマークランナーです。

各試行は同じコミットから作成した新しいGit worktreeで始まり、準備処理、エージェント、検証処理の順に実行されます。変更差分、標準出力、標準エラー、検証結果、禁止操作の検出結果を保存するため、成功した一件だけではなく、成功率、実行時間、変更規模、失敗内容、試行ごとのばらつきを確認できます。

**現在のリリース:** v0.3.0。ソースからインストールします。crates.ioではまだ公開していません。CLIとRust APIはSemantic Versioningに従い、保存形式は独立したスキーマversionで管理します。

> [!WARNING]
> PatchArenaはサンドボックスではありません。エージェント、準備処理、検証処理は、PatchArenaと同じOS権限で動きます。信頼できない入力を扱う前に、[セキュリティ](#セキュリティ)と[脅威モデル](docs/threat-model.md)を確認してください。

## PatchArenaを使う理由

エージェントの紹介では、成功した差分だけが示され、失敗した試行、実行環境、検証出力、正確なリポジトリのコミットが省略されることがあります。PatchArenaは、タスク、基準コミット、実行条件、差分、ログ、検証結果を一緒に残し、あとから確認できる形にします。

想定している用途:

- 同じ修正タスクを複数回実行し、成功率とばらつきを調べる。
- 複数のエージェントを同じタスクと検証条件で比較する。
- `AGENTS.md`などのリポジトリ指示がある場合と、隠した場合を比較する。
- 以前は通っていたタスクを再実行し、エージェントや設定の変更による回帰を確認する。

PatchArenaは、モデルの総合順位、統計的有意性、信頼できないコードの安全性を保証しません。タスク設計、検証内容、反復数、ホストの隔離は利用者が決める必要があります。

## 実行の流れ

```text
バージョン管理されたタスク + 固定したHEAD + 有効なポリシー
                              │
                              ▼
                  試行ごとのdetached worktree
                              │
                  setup → agent → verify
                              │
                              ▼
               diff + logs + audit + result.json
                              │
                       compare / report
```

すべての試行は同じコミットから始まります。コマンドは暗黙のシェルを使わず、実行ファイルと引数の配列として起動。出力には上限を設け、終了後に一時worktreeを削除します。

## 主な機能

- `patcharena init` — リポジトリ内の設定と保存先を作成。
- `patcharena task add/list` — YAML形式のタスクを作成・一覧表示。
- `patcharena doctor` — Git、Rust、worktree、書き込み権限を確認。
- `patcharena agent list/doctor` — エージェントCLIの検出と診断。
- `patcharena run` — 一つのタスクを指定回数実行。
- `patcharena battle` — 同じコミットから複数エージェントを順番に実行。
- `patcharena suite add/list/run/resume/report` — 複数タスクと複数エージェントの組み合わせを管理。
- `patcharena compare` — 保存済みの実行グループを比較。
- `patcharena report` — Markdown、JSON、単体HTMLのレポートを生成。

組み込みアダプターはCodex CLI、Claude Code、Gemini CLIに対応しています。シェルを介さない独自エージェントも、リポジトリ単位で設定できます。

## 必要環境

- LinuxまたはWSL2。
- Git。
- Rust 1.85.0以降。
- 実際のベンチマークでは、Codex CLI、Claude Code、Gemini CLI、または設定済みの独自実行ファイル。

PatchArena本体のビルドとテストに、各エージェントCLIは不要です。

## インストール

```bash
./prepare.sh
cargo install --path crates/patcharena-cli --locked
patcharena --help
```

`prepare.sh`は前提コマンドを確認し、依存関係の取得、ビルド、テスト、静的検査を実行します。`sudo`、パッケージの自動インストール、ユーザーのGit設定変更は行いません。

## クイックスタート

評価したいGitリポジトリ内で実行します。

```bash
patcharena init
patcharena doctor

printf '%s\n' \
  'CSV出力時に各recordの末尾へ改行を1つだけ出力してください。' \
  > prompt.md

patcharena task add \
  --id csv-newline-regression \
  --prompt-file prompt.md \
  --verify "cargo test csv_export"

patcharena agent list
patcharena agent doctor codex
patcharena run --task csv-newline-regression --agent codex --repeat 3
```

実行結果は`.patcharena/`へ保存されます。タスクとスイートの定義はGitで管理できますが、ログ、差分、実行結果、秘密情報を含む可能性がある生成物は通常コミットしません。

## タスク定義

```yaml
id: csv-newline-regression
prompt: |
  CSV出力時に各recordの末尾へ改行を1つだけ出力してください。

setup:
  commands:
    - cargo build

verify:
  commands:
    - cargo test csv_export
    - cargo clippy --all-targets -- -D warnings

limits:
  timeout_seconds: 600
  max_output_bytes: 10485760
  max_changed_files: 8
  max_diff_lines: 500

forbidden:
  commands:
    - git push
    - cargo publish
  paths:
    - .git
    - .env
```

コマンド文字列は、実行ファイルと引数へ分割されます。パイプ、リダイレクト、変数展開などのシェル構文は評価しません。`sh -c`のようにシェルを明示した場合は、その解析と危険性をシェル側へ委ねます。

## 記録する内容

各試行にはUUIDが割り当てられ、次の情報を保存します。

- 成否と終了ステータス。
- 開始時刻、終了時刻、経過時間。
- 変更ファイル数、追加行数、削除行数。
- 準備処理と検証処理の結果。
- 上限付きの標準出力と標準エラー。
- Git差分。
- 禁止コマンド・禁止パスの検出結果。
- タスク、エージェント、結果形式のversion。
- 正確な`HEAD`と、タスク・有効ポリシーから計算した比較用識別子。

ログ、監査記録、差分は、そのまま公開できるように無害化されたデータではありません。共有前に秘密情報が含まれていないか確認してください。

## スイート

スイートは、複数のタスクとエージェントをまとめて実行するための定義です。

```bash
patcharena suite add --id core --task task-a --task task-b
patcharena suite run --suite core --agents codex,claude --repeat 3 --dry-run
patcharena suite run --suite core --agents codex,claude --repeat 3
patcharena suite resume --run <suite-run-id>
patcharena suite report --run <suite-run-id> --format html --output report.html
```

`--dry-run`はエージェントを起動せず、タスク数、エージェント数、反復数、指示の有無、合計実行回数を表示します。合計作業量は`tasks × agents × repeat`で、誤操作による費用の増加を抑えるため1,000回を上限にしています。この値は金額の見積もりではありません。

## 比較とレポート

保存済みの実行グループは、エージェントを再実行せずに比較できます。

```bash
patcharena compare \
  --baseline BASELINE_GROUP_ID \
  --candidate CANDIDATE_GROUP_ID \
  --output comparison.json
```

比較できるのは、完了済みで、要求した試行数と実際の試行数が一致し、タスク、基準コミット、有効ポリシー、試行数が互換なグループだけです。

外部CDNを使わない単体HTMLレポートも生成できます。

```bash
patcharena report \
  --format html \
  --group GROUP_ID \
  --output patcharena-report.html
```

レポートは成功率、実行時間、変更規模、検証内容、失敗、禁止操作、試行ごとの証拠を表示します。PatchArena自身は勝者や統計的有意性を判定しません。

## セキュリティ

Git worktreeは試行の再現性を高め、主要な作業ディレクトリへの意図しない変更を減らします。ただし、Git object、ref、リポジトリ設定を共有するため、ファイルシステムやGitのセキュリティ境界にはなりません。

信頼できないタスク、リポジトリ、エージェントを実行する場合は、次の隔離を別に用意してください。

- 権限を持たない専用ユーザー。
- 空のホームディレクトリ。
- 認証情報やエージェントソケットを置かない環境。
- ネットワーク制限。
- CPU、メモリ、プロセス数を制限した一時VMまたはコンテナ。

禁止操作の検出は、実行後に確認できる証拠を増やすための機能です。危険な操作の実行そのものを完全に防ぐものではありません。

脆弱性は[SECURITY.md](SECURITY.md)の手順で報告してください。前提と残る危険性は[docs/threat-model.md](docs/threat-model.md)に記載しています。

## 開発

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo build --locked --workspace --release
```

変更前に[CONTRIBUTING.md](CONTRIBUTING.md)、[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)、[AGENTS.md](AGENTS.md)、[architecture](docs/architecture.md)、[threat model](docs/threat-model.md)を確認してください。

## ライセンス

[Apache License 2.0](LICENSE)で公開しています。
