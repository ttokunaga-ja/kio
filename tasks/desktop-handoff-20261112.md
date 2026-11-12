# kio：個人デスクトップでの再開と見直し（2026-11-12）

この文書は kio のセッション専用です。personaCorpus の作業は personaCorpus のセッションで行い、ここでは扱いません。役割とモデルは `AGENTS.md` の「エージェント運用」に従います。kio には常駐の監視を置きません。

- **Fable 5.2 のセッション**：「3. 見直し項目」を調べ、提案し、計画・仕様・規則の修正まで行う。
- **Opus 5.5 のセッション（統括）**：決まった計画に沿って、開発・監査・検証を Codex に委譲し、結果を確かめ、統合し、commit と push を判断する。

## 1. 現状（2026-11-12 時点の実測）

- **移行**：Mac から移行済み。main は `4992971` に AGENTS.md の commit `ef0db73` が加わった状態（未 push）。
- **ローカルにしかない branch**：`codex/kio-rc-post-release-hardening`（main より 3 commit 先）と `codex/kio-storage-policy-20261005`（同 1 commit 先）。どちらも remote にはない。
- **`../kio-r2`**：`win/own-objects-only` の worktree。remote と同じ状態。
- **未追跡のもの**：`eval/repin/`、`scratch/`、`tasks/` の引き継ぎメモ 4 件。メモは過去の時点のもので、内容が今も正しいとは限らない。
- **Codex worktree のメモ**：`../migration-from-mac/kio-codex-worktrees/` に置いてある。
- **`scratch/p12/.../node_modules`**：Mac の Codex runtime を指すシンボリックリンクで、WSL では使えない。

- **検証の結果**
  - Rust は 1.98.0。
  - `cargo fmt --all -- --check` と `cargo clippy --workspace --all-targets --locked -- -D warnings`：成功。
  - persona W0：成功。
  - `cargo test`：`systemd-run --user --scope -p Delegate=yes cargo test --workspace --all-targets --locked --no-fail-fast` で、テストバイナリ 112 本・3,653 件がすべて成功（2026-11-12）。

- **WSL で検証するときの注意**
  - Linux のレンダラー隔離は bubblewrap と、委譲された cgroup v2 を前提にしている。
  - bubblewrap は導入済み。
  - `wsl -e` から起動したプロセスは systemd の cgroup の外で動くため、テストは `systemd-run --user --scope -p Delegate=yes cargo test --workspace --all-targets --locked` で実行する。
  - Codex のサンドボックス（workspace-write）の中では `/` と `/home` の所有者が UID 65534 に見え、kio の所有者検査（`crates/kio-core/src/platform/unix/private_fs.rs`）で多数のテストが失敗する。そのため、`cargo test` と synthetic-history を Codex に実行させるときは、サンドボックスの外で実行する方法を統括が決める（例：統括自身が実行する、限定したコマンドだけをサンドボックスの外で実行させる）。
  - OCR の検証用に、`uv` と `experiments/ocr-verification/.venv`（Python 3.12）を用意した。

- **デスクトップ単独では実行できない検証**
  - `v1-local-acceptance`：Actions package、Tailscale、SSH の資格情報、GPU を使う経路が必要。
  - provider・release 系：外部の資源と秘密情報が必要。
  - macOS・Windows でのネイティブ検証：WSL ではできない。

- **既知の問題**
  - `kio-code-limits`：共通コードが 111,757 行で、上限の 106,000 行を超えている（CI はこの状態を許容しており、過去の記録にもある）。
  - 計画の入口は `tasks/v1-implementation-progress.md` と `tasks/v1-closeout-execution-2026-10-03.md`。後者には、残作業と、`docs/05` の paging について資料どうしの矛盾が記録されている。

- **環境構築と検証の報告**：`../migration-from-mac/desktop-jobs/kio.out.md`

## 2. 統括（Opus）の手順

- **Codex の起動**：`codex exec -m <model> -c model_reasoning_effort=medium --sandbox workspace-write -C ~/dev/github.com/ttokunaga-ja/kio "<brief>" < /dev/null`
  - stdin を閉じないと入力待ちで止まる。
  - 長い作業は tmux で動かす。
  - WSL はアイドルになっても止まらない設定にしてある。
- **モデルの使い分け**
  - 実装：`gpt-6.1-terra`
  - 定型の検証・探索：`gpt-6-luna`
  - 重いレビューと、Terra で解決しない実装：`gpt-6.1-sol`
- **push**：kio は公開 repo で、push すると Actions（ci 等）が動く。push の前に、ローカルで同じ検証を通す。

## 3. 見直し項目（Fable が調べ、提案し、実行する）

Fable は自由に調べ、提案し、実行してよい。ただし次は事前にユーザーの確認を取る。
- force push や履歴の改変
- release・deploy、秘密情報・外部サービス・課金に関わること
- AGENTS.md のうち、モデル・権限に関わる変更

1. **実装が遅れている原因の分析**
   - 計画・仕様と、実装の状況との差を調べる。
   - 受入の仕組み（v1-acceptance 系の workflow と、外部資源に頼っている部分）を調べる。
   - 検証にかかる時間を調べる。
   - 資料どうしの矛盾、散らばった branch・worktree・引き継ぎメモを調べる。
   - 過去に Codex が自分で足した、厳しすぎる停止規則や承認手続きがないかを調べる（`tasks/` と `docs/` の規則）。
2. **実装計画の立て直し**
   - 残っている範囲を絞り込み、優先順位をつけ直す。
   - 受入基準を、デスクトップで回せるものと外部資源が要るものに分け、回し方を決める。
   - 計画は Codex（まず Terra）に渡せる大きさのタスクに分ける。
3. **検証の回し方**
   - WSL で CI と同じ検証を回す手順を決める（上の systemd scope、Codex サンドボックスの問題への対応）。
   - 高いコストのかかる検証を、どのタイミングで実行するかを決める。
4. **後片付け**
   - ローカルにしかない branch 2 本と kio-r2 を、取り込むか捨てるか決める。
   - 古い引き継ぎメモを整理する。
   - `kio-code-limits` の上限超過をどう扱うか決める。
   - `docs/05` の矛盾を解消する。
5. **personaCorpus との関係**：kio の検証にどの corpus をどう使うかは、kio の要件として整理する。personaCorpus 側への依頼は、personaCorpus のセッションに渡す（このセッションでは personaCorpus のファイルを変更しない）。
