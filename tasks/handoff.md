# 作業環境の引き継ぎ

**現在の作業環境：Mac**（2026-11-21 から）。

kio の作業環境は時期によって丸ごと切り替わる（D-20261121-02）。通常は Mac、学会などで Mac を安定して使えない期間は個人デスクトップ WSL を使う。切り替えは利用者が指示し、作業内容によって環境を使い分けない。同時に両方の環境で作業しない。どちらの環境でも、この文書と `git pull` で作業を再開できる。切り替えるときは「6. 環境の切り替え」の手順に従い、この冒頭と「1」を書き換える。

## 1. 最後に環境を切り替えた時点の状態（2026-11-21、WSL → Mac）

- 計画（`tasks/v1-plan.md`）の残りは **F4（正式な受入）だけ**である。F3（実装完了の判定）は main の `2c270c5` で完了した。ローカル検証と、通常の CI の 3 つの OS の全 job（run 44786625755）が成功している。
- `2c270c5` の後の commit は、計画の記録と、この引き継ぎの文書だけである。移管の時点では、最新の commit の通常の CI が実行中だった（`ci.yml` の concurrency により、`87910b1` の run は取り消される）。正式な受入の候補にできるのは、CI の push の run が成功した main の commit だけである。再開したら最初に `gh run list -L 5` で結果を確かめる。
- 次の作業は [v1 正式受入](v1-acceptance.md)。構成の整理を統合してから、候補の CI と外部資源の準備を確認し、51 件と集約を一度だけ回す。GPU ホストの決定と接続前提は正式受入の手順に記録する。
- GPU 受入ホスト（D-20261121-05）：研究室 4060 と個人デスクトップ 4070 の両方に `scripts/v1-local-gpu/host/` で構築済み（2026-11-21。管理者 SSH、`kio-ci` の制限、2 段 SSH で候補未配置の拒否、CUDA スモークを確認）。Environment `v1-local-acceptance` は両台の host key と新しい受入鍵を登録し、`KIO_LAB_JUMP_HOST` は Mac で開発中のため研究室（`100.68.218.37`）。4070 で開発する期間は `100.97.109.109` に書き換える。Tailscale の policy は 2026-11-22 に `tag:kio-ci` から両台の TCP 22 だけを許可した。route-probe（run 44919717116）で `tag:kio-ci` の経路と拒否を確かめた。研究室のネットワークは OCR イメージの Baidu レジストリ（`120.48.94.1:443`）への TCP 接続だけを遮断する（他の中国本土のミラーと 780 MB の取得は成功、2026-11-22）。そのため F4 の GPU 受入は 4070 で行い、`KIO_LAB_JUMP_HOST` は `100.97.109.109` にしている。F4 後に研究室へ戻すには、イメージを digest を保って移す方法か、研究室ネットワークでの許可が要る。S4U の keepalive はサインアウト後が未確認。
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
mkdir -p scratch/runtime/litert
RUNNER_TEMP="$PWD/scratch/runtime/litert" GITHUB_ENV=/dev/null bash scripts/ci/fetch-litert-runtime.sh
export KIO_LITERT_C_API_ZIP="$PWD/scratch/runtime/litert/litert_lm_c_api-1.0.0.zip"
```

この変数がないと、`kio-cli` の build script が失敗する（`KIO_LITERT_C_API_ZIP must name an existing absolute ZIP path`）。2026-11-21 に WSL でこの失敗が起きた。テストを回すシェルでは、毎回この変数を設定する。

## 3. Mac でのローカル検証

LiteRT ZIP を設定したシェルから [ローカル検証スクリプト](../scripts/ci/local-checks.sh) を `bash scripts/ci/local-checks.sh <作業名> all` で実行する。評価・検索・索引・履歴に触れた場合は引数に `synthetic-history` を足す（Linux だけ。Mac では動かないので通常の CI の `synthetic-history-eval` で確かめる）。終了値とログを確認し、実際のホームに kio のデータが作られていないことを確かめる。

- Linux 固有の confinement・cgroup・systemd と Windows の動作は通常 CI の対象 job で確かめる。
- CI の `sudo kio-prepare-macos-sdk` は使い捨て runner 用。手元で必要になった場合は内容を読んで利用者に確かめる。

## 4. Codex への委譲（AGENTS.md「エージェント運用」に従う）

- 実装：`codex exec -m gpt-6.2-sol -c model_reasoning_effort=medium -s workspace-write -C <worktree> --add-dir ~/.cargo -o <報告> "$(cat <指示書>)" < /dev/null`。作業量が「小」の単位は `gpt-6.1-terra` から始める。
- レビュー：同じ形で `-s read-only` にする。
- 続きの依頼：`codex exec resume <session id> -m gpt-6.2-sol -c model_reasoning_effort=medium -c sandbox_mode=workspace-write -c "sandbox_workspace_write.writable_roots=[\"$HOME/.cargo\"]" -o <報告> "<依頼>" < /dev/null` を worktree の中で実行する。`resume` は `-s`・`-C` を受け付けない。session id は最初の実行のログの `session id:` の行にある。
- worktree：`scratch/wt/<単位>` に `wt/<単位>` の branch で作る。統括が差分を確かめ、`git add -A` で commit し（範囲を絞ると `Cargo.lock` が漏れる）、main に `--no-ff` で merge してから、worktree と branch を消す。
- 個別の指示書は [委譲用の雛形](agent/brief-template.md) に作業範囲と受入条件を記入して渡す。
- Codex のサンドボックスでは `.git` に書けない。削除・stage・commit は統括が行う。Codex には具体的な対象の一覧（数十件まで）を渡す。
- Codex の報告のうち、完了・一致・失敗の原因などの主張は、統括がコマンドで確かめてから採用する。

## 5. これまでに分かった注意点

- 時間制限つきのテスト（隔離、renderer、同期待ち）は、マシンの負荷が高いと失敗する。失敗したら、まず負荷と他の作業を確かめ、該当するテストだけを 1 本ずつ回し直す。1 本ずつでも再現する失敗は本物の退行である。
- 回している `cargo test` を止めるときは、rustup の中継ではなく、本物の cargo とその子孫を止める。
- 検証のコマンドを `| tail` につなぐと、終了コードは `tail` のものになる。`set -o pipefail` を付けるか、出力をファイルに書いて終了コードを別に取る。
- GitHub の API の rate limit に当たったことがある。CI の完了待ちは、30 分以上の間隔で確かめる（CI 全体は約 85〜90 分で、Windows の remaining の job が最も長い）。
- `ci.yml` は concurrency で前の run を取り消す。CI の途中で push すると、前の run の結果は出ない。

## 6. 環境の切り替え

利用者が切り替えを指示したら、次の順で行う。

1. 離れる環境で未統合の差分・コミットと CI の状態、再開条件を記録する。この文書の §1 に反映し、通常の Git 手順で保全する。
2. 雛形とローカル検証スクリプトは Git で移る。手で移すのは進行中の指示書と未解決の報告だけに絞り、必要な再取得不能の証跡は報告と一緒に移す。双方で SHA-256 を照合する。`.env` は必要時に別送し、受領側で権限を `600` にする。値は表示しない。
3. 移る先で `git pull --ff-only` を行い、保全した差分から必要な worktree を作る。LiteRT ZIP は §2 の手順で `scratch/runtime/litert/` に取得し、ビルドと検証出力を作り直す。
4. 通常のローカル検証と再開条件を確かめ、この文書の冒頭と §1 を更新する。

2026-11-21 の WSL → Mac の切り替えでは作業用ファイル 507 件と `.env` の SHA-256 を照合した。WSL の `scratch/` は保全している。
