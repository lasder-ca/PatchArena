# PatchArena

[![CI](https://github.com/lasder-ca/PatchArena/actions/workflows/ci.yml/badge.svg)](https://github.com/lasder-ca/PatchArena/actions/workflows/ci.yml)

[English](README.md) | **日本語**

PatchArenaは、実際のリポジトリ上でAIコーディングエージェントを再現可能に評価するベンチマークランナーです。

バージョン管理された修正タスクを新しいGit worktreeで実行し、エージェントの挙動を記録して、検証結果を機械可読な証拠として保存します。同じタスクを複数回実行することで、単発の成功例だけではなく、成功率、実行時間、パッチ規模、検証失敗、ポリシー違反、実行間のばらつきを比較できます。

**プロジェクトの状態:** ソースから利用するプレリリース版です。crates.ioパッケージと安定互換性の保証はまだありません。ディスク上の形式にはバージョンが付いており、互換性のないデータを誤って読み込まず明示的に失敗します。

[クイックスタート](#クイックスタート) · [タスク形式](#タスク定義) · [レポート](#htmlレポートの例) · [セキュリティ](#セキュリティ) · [コントリビューション](CONTRIBUTING.md)

> [!WARNING]
> PatchArenaは完全なsandboxではありません。エージェント、setup、verifyの各プログラムは、PatchArenaプロセスと同じOS権限で動作します。信頼できない入力を扱う前に、[セキュリティ](#セキュリティ)と[脅威モデル](docs/threat-model.md)を確認してください。

## PatchArenaを使う理由

エージェントのデモでは、成功した1件のパッチだけが示され、失敗した試行、実行環境、検証出力、正確なリポジトリrevisionが省略されることがあります。PatchArenaは、それらの入力と結果を明示します。ローカル実験、エージェント評価、回帰試験、リポジトリ指示あり・なしの比較など、証拠を後から確認できる必要がある用途を想定しています。

PatchArenaは、モデルの総合ランキング、統計的有意性、信頼できないコードの安全性を保証しません。再現可能な実行制御と証拠収集を提供しますが、実験設計とホスト隔離は利用者の責任です。

## 動作の流れ

```text
バージョン管理されたタスク + commit済みHEAD + 有効ポリシー
                              │
                              ▼
                反復ごとのdetached worktree
                              │
                 setup → agent → verify
                              │
                              ▼
             diff + logs + audit + result.json
                              │
                       compare / report
```

各反復は同じ固定commitから開始します。PatchArenaは暗黙のshellを使わずにコマンドを実行し、上限付きの証拠を記録し、一時worktreeを削除して、各runをUUID配下へ保存します。run groupには要求したサンプル数と完了状態が記録されます。

## 現在の対応範囲

初期バージョンでは次のコマンドを提供します。

- `patcharena init` — 既存ファイルを上書きせず、リポジトリローカルの設定と状態を作成
- `patcharena task add` / `patcharena task list` — YAMLタスクの作成と一覧表示
- `patcharena doctor` — Git、リポジトリ、Codex CLI、Rust、worktree、書き込み権限を確認
- `patcharena run` — Codex CLIでタスクを1回以上実行して証拠を保存
- `patcharena compare` — 保存済みrun groupを比較
- `patcharena report` — Markdown、JSON、単一HTMLレポートを生成

本番向けエージェント統合はCodex CLIのみです。テストは決定的な偽エージェントを使うため、Codexや外部サービスを必要としません。

## 記録する内容

各runでは次の情報を記録できます。

- 成否とコマンド終了ステータス
- 開始・終了時刻と経過時間
- 変更ファイル数、追加行数、削除行数
- setupとverifyの結果
- 上限付きstdout / stderr
- 生成されたGit patch
- 禁止コマンド・禁止パス違反
- タスク、エージェント、結果schema version
- 正確な`HEAD` commitとタスク／有効ポリシーfingerprintを含むbenchmark identity

反復結果を集約すると、成功率、実行時間中央値、ばらつきを確認できます。通常実行と`--without-instructions`実行を分けることで、他の入力を利用者が管理している前提のもと、`AGENTS.md`などのリポジトリ指示あり・なしを比較できます。

## 必要環境

- LinuxまたはWSL2（主な対応環境）
- Git
- Rust **1.85.0**以上（MSRV、Rust 2024 edition）
- 本番の`run`コマンドを使う場合のみCodex CLI

PatchArena自体のbuildとtestにはCodex CLIは不要です。

## インストール

現在はソースcheckoutからインストールします。

```bash
./prepare.sh
cargo install --path crates/patcharena-cli --locked
patcharena --help
```

`prepare.sh`は前提コマンドを確認し、依存取得、build、test、lintを実行します。`sudo`、パッケージの自動インストール、ユーザーのGit設定変更は行いません。開発中はインストールせず、`cargo run -p patcharena-cli -- <arguments>`でも実行できます。

ソース版を更新する場合は、利用したいrevisionをpullまたはdownloadし、変更内容と検証結果を確認してから次を実行します。

```bash
cargo install --path crates/patcharena-cli --locked --force
```

## クイックスタート

評価対象のGitリポジトリ内で実行します。

```bash
patcharena init
patcharena doctor

printf '%s\n' \
  'CSV exporterが各recordの末尾に改行を1つだけ出力するよう修正してください。' \
  > prompt.md

patcharena task add \
  --id csv-newline-regression \
  --prompt-file prompt.md \
  --verify "cargo test csv_export"

patcharena task list
patcharena run --task csv-newline-regression --agent codex --repeat 3
```

`run`はgroup UUIDを表示します。`compare`や対象指定`report`で使うため保存してください。生成データは`.patcharena/`配下へ保存されます。タスクYAMLはversion管理できますが、run artifactは通常ローカルのままにします。

リポジトリ内の`AGENTS.md`をエージェントから一時的に隠した比較groupを作るには、`--without-instructions`を追加します。setup後にworktreeを走査し、追跡外・ignore対象を含む通常ファイルの`AGENTS.md`を隠し、verify前に復元します。symlink directoryは追跡せず、走査は100,000 entryまでです。上限超過や`AGENTS.md` symlinkを検出した場合は、不完全な状態で続行せずrunを失敗させます。

このオプションはcontextが完全にないエージェントを作るものではありません。worktree外の指示、別名の指示ファイル、ユーザー／グローバル設定、エージェント既定値、モデル側context、setupプログラムが既に観測した入力は隠しません。

`init`は何度実行しても既存の有効な`patcharena.toml`を保持し、安全なmetadata directoryを再利用します。生成されたrun、group、comparison、report artifactと秘密情報はversion管理しないでください。

## コマンド一覧

| コマンド | 用途 |
|---|---|
| `patcharena init` | リポジトリローカルの設定と状態directoryを作成 |
| `patcharena task add` | prompt fileとコマンドから検証済みタスクを作成 |
| `patcharena task list` | タスクIDと制限値を一覧表示 |
| `patcharena run` | 1回以上の隔離された反復を実行 |
| `patcharena compare` | 互換性のある完了groupまたは個別runを比較 |
| `patcharena report` | Markdown、JSON、単一HTMLを生成 |
| `patcharena doctor` | Git、Rust、Codex CLI、worktree、書き込み可能性を確認 |

正確なoption一覧は`patcharena <command> --help`で確認できます。安定したerror categoryの終了コードは、入力またはローカルI/Oが`3`、Gitまたは前提条件が`4`、runner失敗が`5`、reportまたはcomparison失敗が`7`です。引数parse errorにはClapの標準終了コードを使います。

## タスク定義

タスクは`.patcharena/tasks/<id>.yaml`へ保存します。setup、verify、resource／patch規模制限、禁止操作を定義できます。

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

コマンド文字列は実行ファイルと引数配列へ分割されます。pipe、redirect、変数展開などのshell operatorは評価しません。`sh -c`などのshellを明示的に起動した場合、そのparseとriskはshell側へ委譲されます。

機械生成タスクではtokenizeを避けた構造化形式も利用できます。

```yaml
verify:
  commands:
    - program: cargo
      args: ["test", "csv_export"]
```

リポジトリ既定値は[`patcharena.toml.example`](patcharena.toml.example)に記載しています。数値の既定値は実行時の安全上限でもあり、タスクは制限を厳しくできますが緩和できません。実効値はタスク値とproject値の小さい方です。timeoutと出力上限は各setup、agent、verify processへ個別に適用され、変更ファイル数とdiff行数は最終patchへ適用されます。実効ポリシーが変わるとbenchmark fingerprintも変わります。

## 結果

各反復にはUUIDが割り当てられます。group metadataは要求反復数と`running`、`completed`、`aborted`状態を記録し、最初の反復前に作成され、完了した反復ごとにatomic更新されます。突然のhost crashで`running`のまま残ったgroupは、成功扱いされません。

```text
.patcharena/runs/<run-id>/
├── result.json
├── stdout.log
├── stderr.log
├── changes.diff
└── audit.jsonl
```

`result.json`では`schema_version`が必須です。benchmark identityは、結果集合を比較可能か判断するために使います。JSON Lines形式のaudit artifactには、各phaseで起動したコマンドの証拠が記録されます。log、audit、patchはsanitize済み公開物ではありません。共有前に秘密情報を確認してください。

## runの比較

エージェントを再実行せず、保存済みgroup IDを比較します。

```bash
patcharena compare \
  --baseline BASELINE_GROUP_ID \
  --candidate CANDIDATE_GROUP_ID \
  --output comparison.json
```

1 sampleとしてrun IDも指定できますが、通常はgroup IDを使います。両方が完了済みで、観測run数と要求run数が一致し、task ID、benchmark identity、sample sizeが同じ場合のみ比較できます。完了metadataのないlegacy groupやidentityのない不正recordは比較しません。

identityは正確なrepository `HEAD`と、タスク定義・解決済み実行ポリシーのSHA-256 fingerprintを組み合わせます。比較対象にできるよう、選択したagentとinstructions on/off条件は意図的に含めません。これは互換性guardであり、署名済みattestationや完全な環境固定ではありません。toolchain、依存、agent/model設定、credential、network responseなどは利用者が管理する必要があります。

比較結果には成功率、実行時間中央値、変更ファイル数、diff行数、verify失敗、禁止操作検出、反復間のばらつきが含まれます。

## HTMLレポートの例

外部CDNを使わない、screenshot向けの単一HTMLを生成できます。

```bash
patcharena report \
  --format html \
  --group GROUP_ID \
  --output patcharena-report.html
```

レポートにはtask、agent、完了状態、要求／観測反復数、成功率、実行時間、patch規模、verify詳細、error、policy違反、runごとの証拠が表示されます。`running`、`aborted`、legacy groupも確認できますが、比較対象にはできません。READMEには実在しないbenchmark値を掲載していません。

```bash
patcharena report --format json --output patcharena-report.json
patcharena report --format markdown --output patcharena-report.md
```

## セキュリティ

detached worktreeは反復性を改善し、primary checkoutの意図しない変更を減らします。timeout、出力上限、環境変数allowlist、path validation、policy checkも一般的な失敗を減らします。ただしlinked worktreeはGit object、ref、repository設定をprimary repositoryと共有します。Git metadataと禁止pathの一部を実行後に比較しますが、これは上限付きの事後検出であり、filesystemやGitのsecurity boundaryではありません。

Unixではsetup、agent、verify processを個別のprocess groupで実行します。timeout時とdirect childの通常終了後に残存group memberの終了を試みます。別sessionやprocess groupへ離脱した子孫は残る可能性があります。native Windowsでは現在direct childのみを終了し、Job Objectは使用していません。

信頼できないbenchmarkには、権限のない専用ユーザー、cleanなhome、credentialやagent socketなし、network制御、OS resource制限を設定した一時VMまたはcontainerを利用してください。禁止操作検出は監査可能な多層防御であり、実行防止を保証しません。run artifactにはsource、prompt、URL、環境由来text、その他の秘密が含まれる可能性があります。

脆弱性の報告は[SECURITY.md](SECURITY.md)、前提と残存riskは[docs/threat-model.md](docs/threat-model.md)を参照してください。

## 現在の制限

- 本番向けagentはCodex CLIのみ
- 主対象はLinux / WSL2で、native Windowsのworktreeとprocess treeは継続的にテストしていない
- Git worktreeと事後checkはfilesystem、process、network sandboxではない
- Unixのprocess-group cleanupはbest effortで、離脱した子孫が残る可能性がある。Windowsはdirect childのみ終了
- CPU、memory、process数、network traffic、child processが直接書き込むfile sizeは制限しない
- 内部Git subprocessには独立timeoutがない
- Git ignore対象fileや未初期化submoduleの内容はdiff証拠に含まれない。禁止path snapshotにも件数・容量上限がある
- policy matchingでは間接的または意味的に等価な危険操作をすべて認識できない
- task commandは引用付き引数に対応するが、shellを明示しない限り一般的なshell構文には対応しない
- reportはlocal artifactのみで、hosted dashboardやremote result serviceはない
- benchmark identityは`HEAD`、task、有効PatchArena policyを固定するが、完全な実行環境は固定しない

## ロードマップ

- native Windows Job Object、離脱したUnix子孫への対策、container profile
- worktree lifecycleが安定した後のnative Windows CI
- `AgentRunner`を利用した本番agent adapterの追加
- instructions on/off比較の実験metadata改善
- schema migrationと統計summaryの拡充
- artifact retentionとopt-in redaction

ロードマップは予定であり、releaseを約束するものではありません。

## コントリビューション

issueと焦点を絞ったpull requestを歓迎します。変更前に[CONTRIBUTING.md](CONTRIBUTING.md)、[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)、[AGENTS.md](AGENTS.md)、[architecture](docs/architecture.md)、[threat model](docs/threat-model.md)を確認してください。security reportは公開issueではなく[SECURITY.md](SECURITY.md)に従ってください。

最低限、次を実行します。

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo test --locked --workspace --all-features
cargo build --locked --workspace --release
```

API key、実run log、`.env`、生成された`.patcharena`データ、再現可能なrecordで裏付けられていないbenchmark claimを含めないでください。利用者に見える変更は[CHANGELOG.md](CHANGELOG.md)の`Unreleased`へ記録します。

## ライセンス

[Apache License 2.0](LICENSE)で提供します。
