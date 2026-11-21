# 作業環境の引き継ぎ

2026-11-21、kio の作業を個人デスクトップ WSL から Mac に移した（D-20261121-01）。WSL の clone と `scratch/` は消していない。どちらの環境でも、この文書と `git pull` で作業を再開できる。同時に両方の環境で作業しない。

## 1. 移管の時点の状態

- 計画（`tasks/v1-plan.md`）の残りは **F4（正式な受入）だけ**である。F3（実装完了の判定）は main の `2c270c5` で完了した。ローカル検証と、通常の CI の 3 つの OS の全 job（run 44786625755）が成功している。
- `2c270c5` の後の commit は、計画の記録と、この引き継ぎの文書だけである。移管の時点では、最新の commit の通常の CI が実行中だった（`ci.yml` の concurrency により、`87910b1` の run は取り消される）。正式な受入の候補にできるのは、CI の push の run が成功した main の commit だけである。再開したら最初に `gh run list -L 5` で結果を確かめる。
- 利用者の決定：F4 の 51 件は、外部の資源の準備がすべて整ってから一度にまとめて回す。3 つの OS の 42 件だけを先に回すことはしない。
- 次の作業：`tasks/f4-external-status-check.md` の手順で、利用者と一緒に（Codex の Chrome 操作で）外部の資源の状態を確かめる。結果は `scratch/reports/f4-external-status.md` に出る。それを読んで、F4 の準備の手順と所要時間を利用者に示す。
- 大事な未確定の点：GPU 用の Environment `v1-local-acceptance` の接続先は `kio-lab-windows`（100.68.218.37）である。個人デスクトップではない。どの端末を GPU の host にするかを利用者に確かめる。経路の文書 `tasks/v1-private-actions-route.md` は 2026-09 時点のもので、lab 側の `kio-ci` アカウント、SSH の設定、dispatcher の配置を「未適用」としている。
- 観察を続けている事項：`v1_local_http` のテストで「database disk image is malformed」が一度だけ出た。その後は再現していない（64 回と 24 回の並列実行、CI）。再発したらログを残して調べる。

## 2. Mac での開始の手順

```bash
cd ~/dev/github.com/ttokunaga-ja/kio
git switch main && git pull --ff-only
git status --short          # 何も出ないこと
rustup toolchain install 1.98.0 --component rustfmt --component clippy
gh auth status
codex --version
```

EG2 の runtime（LiteRT-LM v0.19.1、C API 1.0.0）の ZIP を一度だけ取得し、build に渡す。

```bash
mkdir -p scratch/litert
RUNNER_TEMP="$PWD/scratch/litert" GITHUB_ENV=/dev/null bash scripts/ci/fetch-litert-runtime.sh
export KIO_LITERT_C_API_ZIP="$PWD/scratch/litert/litert_lm_c_api-1.0.0.zip"
```

この変数がないと、`kio-cli` の build script が失敗する（`KIO_LITERT_C_API_ZIP must name an existing absolute ZIP path`）。2026-11-21 に WSL でこの失敗が起きた。テストを回すシェルでは、毎回この変数を設定する。

## 3. Mac でのローカル検証

Mac では、通常の CI の `macos-security-r23` の job と同じ範囲を確かめられる。

```bash
cargo +1.98.0 fmt --all -- --check
cargo +1.98.0 clippy --workspace --all-targets --locked -- -D warnings
CARGO_PROFILE_TEST_OPT_LEVEL=1 cargo +1.98.0 test --workspace --all-targets --locked --no-fail-fast
cargo +1.98.0 run -q -p kio-ci --bin kio-code-limits --locked -- --root . > scratch/code-limits.json; echo $?   # 0 であること
```

- Linux 固有のコード（confinement、cgroup、systemd の scope を使うテスト）と Windows の動作は、Mac では確かめられない。通常の CI の `rust` と `windows-workspace-tests` の job で確かめる。Linux 固有の変更は、WSL に戻るか、小さい単位で push して CI で確かめる。
- CI の macOS の job は、テストの前に `sudo` で `kio-prepare-macos-sdk` を実行する。これは使い捨ての runner 用の手順である。手元の Mac では、中身を読んで利用者に確かめるまで実行しない。
- synthetic-history（`kio-eval`・検索・索引・履歴に触れた変更の push の前）は、`.github/workflows/ci.yml` の `synthetic-history-eval` の job の手順を、`/tmp` を `scratch/ci-local` に置き換えて回す。WSL ではこれを `scratch/ci-local.sh`（gitignore 済みで、WSL にだけある）にまとめていた。この script の Linux 固有の部分（`target-win` の clippy、confinement の準備）は Mac では動かない。
- 全テストの後に、テストが実際のホームに書き込んでいないことを確かめる。Mac での kio の保存場所を確かめてから、その場所を `ls` する。

## 4. Codex への委譲（AGENTS.md「エージェント運用」に従う）

- 実装：`codex exec -m gpt-6.2-sol -c model_reasoning_effort=medium -s workspace-write -C <worktree> --add-dir ~/.cargo -o <報告> "$(cat <指示書>)" < /dev/null`。作業量が「小」の単位は `gpt-6.1-terra` から始める。
- レビュー：同じ形で `-s read-only` にする。
- 続きの依頼：`codex exec resume <session id> -m gpt-6.2-sol -c model_reasoning_effort=medium -c sandbox_mode=workspace-write -c "sandbox_workspace_write.writable_roots=[\"$HOME/.cargo\"]" -o <報告> "<依頼>" < /dev/null` を worktree の中で実行する。`resume` は `-s`・`-C` を受け付けない。session id は最初の実行のログの `session id:` の行にある。
- worktree：`scratch/wt/<単位>` に `wt/<単位>` の branch で作る。統括が差分を確かめ、`git add -A` で commit し（範囲を絞ると `Cargo.lock` が漏れる）、main に `--no-ff` で merge してから、worktree と branch を消す。
- 指示書には次を必ず書く：作業ディレクトリ（worktree の絶対パス）、読む資料（`AGENTS.md`、`tasks/v1-plan.md` の §3・§4、製品仕様の `docs/01`〜`13`、決定の `docs/decisions.md`）、出力先は worktree の `scratch/` の下だけ、git の commit・push・branch 操作をしない、TDD、旧経路を残さない、行数の上限（`kio-code-limits` が 0 で終わる）、停止してよい条件（取り返しのつかない操作、公開、秘密情報、課金だけ）、最後の報告の形（変更点、Red→Green、検証の結果、保留）。
- Codex のサンドボックスでは `.git` に書けない。削除・stage・commit は統括が行う。Codex には具体的な対象の一覧（数十件まで）を渡す。
- Codex の報告のうち、完了・一致・失敗の原因などの主張は、統括がコマンドで確かめてから採用する。

## 5. これまでに分かった注意点

- 時間制限つきのテスト（隔離、renderer、同期待ち）は、マシンの負荷が高いと失敗する。失敗したら、まず負荷と他の作業を確かめ、該当するテストだけを 1 本ずつ回し直す。1 本ずつでも再現する失敗は本物の退行である。
- 回している `cargo test` を止めるときは、rustup の中継ではなく、本物の cargo とその子孫を止める。
- 検証のコマンドを `| tail` につなぐと、終了コードは `tail` のものになる。`set -o pipefail` を付けるか、出力をファイルに書いて終了コードを別に取る。
- GitHub の API の rate limit に当たったことがある。CI の完了待ちは、30 分以上の間隔で確かめる（CI 全体は約 85〜90 分で、Windows の remaining の job が最も長い）。
- `ci.yml` は concurrency で前の run を取り消す。CI の途中で push すると、前の run の結果は出ない。

## 6. WSL に残したもの

WSL の clone（`~/dev/github.com/ttokunaga-ja/kio`）の、gitignore 済みの `scratch/` に次を残した。repo には入っていないので、Mac にはない。

- `scratch/briefs/`：これまでの Codex への指示書（`common.md` が共通の指示の雛形）。
- `scratch/reports/`：Codex の報告とレビュー、作業記録 `worklog.md`、F2 の食い違いの一覧。
- `scratch/ci-logs/` と `scratch/ci-*.log`：失敗した CI の job のログ。
- `scratch/ci-local.sh`：Linux の CI の job のうち、全テスト以外の手順をまとめた script。
- `scratch/litert/c_api.zip`：EG2 の runtime の ZIP（Mac では 2 の手順で取り直す）。

WSL に戻るときは、WSL で `git pull --ff-only` してから、`AGENTS.md` の「作業場所」と、この文書の冒頭を書き換える。
