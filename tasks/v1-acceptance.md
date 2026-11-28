# v1 の正式受入（F4）

環境・構成の整理をすべて統合してから、同じ候補 SHA でネイティブ 42 件、有料 provider 6 件、認証付き GPU 3 件と最終集約を一度だけ実行する。実装完了の判定は [v1 計画](v1-plan.md) §2 を参照する。

## 実行前の条件と順序

1. [受入ホストの構築](../scripts/v1-local-gpu/README.md#受入ホストの構築)に従って、初期構築用 helper と共有 GPU 照会を root 管理で配置する。初期構築時の候補未配置は、2 段 SSH の `DEPLOYMENT_MISSING` で確認する。GPU 経路の構築・変更時は [GPU 受入](../.github/workflows/v1-local-acceptance.yml) の `operation=route-probe` で、両台への Windows TCP 22 と WSL ホスト鍵の照合、および許可外の経路が到達不能であることを確認する。この診断は有料 provider・台帳の準備に先行できる。
2. `main` の完全な 40 桁 SHA を候補にし、同じ SHA の `ci.yml` の push run が成功したことを確かめる。
3. F4 の前に両台へ同じ候補の dispatcher と固定 OCR 入力を配置する。[候補の固定入力と配置](../scripts/v1-local-gpu/README.md#candidate-bound-deployment-proposal)に従い、D-20261121-05 のホスト選択と固定 OCR image の用意を行う。有料レーンの確認は [provider 予算台帳](acceptance/provider-budget.md) に従う。
4. `main` を ref とし、[ネイティブ受入](../.github/workflows/v1-acceptance.yml) を `candidate_sha` で実行する。3 OS の候補 package が発行された完了済み run の ID を、有料受入の `package_run_id` に渡す。GPU 受入と最終集約はネイティブ run の全件成功を必要とする。A12 は独立に再ビルドしたアーカイブも検証する。
5. [provider 受入](../.github/workflows/v1-provider-acceptance.yml) は `operation=run`、`campaign_id`、`candidate_sha`、`package_run_id` を使う。[GPU 受入](../.github/workflows/v1-local-acceptance.yml) は `operation=run` と同じ `candidate_sha`・`package_run_id` を使う。台帳の予約は失敗・結果不明も消費として扱う。
6. [最終集約](../.github/workflows/v1-acceptance-verify.yml) を同じ `candidate_sha` で実行する。成功 run が一意なら自動で選ぶ。複数ある場合は該当する `ci_run_id`、`native_run_id`、`provider_run_id`、`local_run_id` を指定する。集約成功と 51 件の受領書、候補 SHA、使用した run ID とパッケージを記録する。

## ローカル A08 の証跡

各 OS で候補 package を照合し、GPU リース取得前に独立した runner 上の作業領域で CPU の EG2 を実行する。公開テキストと PNG のオフライン索引、検索、画像 open、再構築、native producer と台帳 0 を確認する。モデルの初回インストールには公開配布元への到達が必要。OCR 側は TLS トンネルで HTTP 200 を確認し、実 OCR の正規化、検索、grant の取り消しと台帳 0 を検証する。

EG2 の成功記録はバイナリ・fixture・プロファイルを結び付ける。OCR 側がこの記録と実測 service identity を照合し、両方のツールを含む `kio.acceptance.receipt/v4` を OS ごとに 1 件作る。OCR と EG2 は別の作業領域を使う。Workflow は expected・receipt・EG2 記録・初期 identity・OCR 観測の存在を各 OS で検査し、リース回収も成功して初めて合格する。最終集約はバインディングを照合し、同一 run の最新 attempt に完全一致する Linux・macOS・Windows の受領書を各 1 件取り込む。ローカル受入を再実行するときは 3 OS を同じ attempt にそろえる。

## 外部資源の読み取り確認

利用者と管理画面を確認し、鍵の登録・更新日時・状態だけを記録する。秘密値や認証画面の入力内容は記録しない。再ログイン・多要素認証は利用者が操作する。この段階の権限は閲覧までとし、設定の保存、資格情報の発行、課金・予算の変更、SSH 接続、workflow dispatch・rerun は後続の準備・実行段階で扱う。

### 確認済みの記録と未確認の前提

2026-11-21 に三つの Environment が `main` 専用であることを確認した。これらは当日の記録であり、実行前に再確認する。

| Environment | 確認対象 |
| --- | --- |
| `v1-provider-acceptance` | `GEMINI_API_KEY`、`MISTRAL_API_KEY` |
| `v1-provider-ledger-authority` | `KIO_LEDGER_APP_PRIVATE_KEY`、`KIO_LEDGER_APP_CLIENT_ID`、`KIO_LEDGER_APP_ID`、`KIO_LEDGER_APP_SLUG`、`KIO_LEDGER_INSTALLATION_ID` |
| `v1-local-acceptance` | `KIO_LAB_SSH_PRIVATE_KEY`、`KIO_LAB_JUMP_HOST`、`KIO_LAB_JUMP_USER`、`KIO_LAB_KNOWN_HOSTS`、`KIO_TS_CLIENT_ID`、`KIO_TS_AUDIENCE` |

**GPU ホストは研究室の RTX 4060 と個人デスクトップの RTX 4070。** 両台とも WSL 内の kio-test 専用 rootless Docker Engine と NVIDIA Container Toolkit の CDI を使う。Mac 作業中は 4060、停止中・利用不可なら 4070、4070 で作業中は 4070 を使う。両台の構築・接続確認は実行前に行う。同一の受入鍵と両台の Windows・WSL ホスト鍵を登録し、run 開始前に `KIO_LAB_JUMP_HOST` のみを手動で切り替える。3 OS の実行とリース回収が完了するまで接続先を固定する。[構築・切り替え手順](../scripts/v1-local-gpu/README.md#受入ホストの構築)

### 確かめること

各項目について、**状態**（使える／使えない／不明）と**根拠**（どの画面で何を見たか）を書く。

#### A. 有料 provider（6 件）

1. **Google AI Studio / Google Cloud（Gemini）**
   - Environment の `GEMINI_API_KEY` に当たる鍵が、今も有効なものとして一覧にあるか（作成日 2026-09-08 前後、末尾表示が出るならその一致）。削除・無効化・制限の変更がないか。
   - 鍵の属するプロジェクトで課金が有効か。無料枠のままか。
   - embedding のモデル `gemini-embedding-2` が使えるか。課金設定を初回・設定変更・障害時に確認する。受入台帳の上限は provider ごとに全キャンペーン累計 USD 10。
2. **Mistral（La Plateforme の console）**
   - `MISTRAL_API_KEY` に当たる鍵が有効か（作成日 2026-09-08 前後）。
   - workspace の課金・支払い方法が有効か。月の上限・残高。初回・設定変更・障害時に Batch と正式受入の固定モデルが利用可能かを確認する。モデルは [受入コード](../crates/kio-eval/src/acceptance_provider.rs) を参照する。
3. **GitHub App（ledger 用）**
   - GitHub の Settings → Developer settings → GitHub Apps に、`KIO_LEDGER_APP_SLUG` の名前の App があるか。repo `ttokunaga-ja/kio` に install されていて、その installation の権限が Contents の read/write を含み、対象 repo が kio だけか。
   - App の秘密鍵の登録日と状態を一覧で確かめる。

#### B. GPU 経由のローカル（3 件）

4. **Tailscale の管理画面**
   - Access controls の policy に `tag:kio-ci` の `tagOwners` と、`tag:kio-ci` から両 GPU ホストの `tcp:22` だけへ到達できる grant があるか。wildcard の grant（`src:["*"], dst:["*"]`）が残っているか。報告には該当する規則と判定根拠を書く。
   - Settings の OAuth clients / Trust credentials（workload identity federation）に、`KIO_TS_CLIENT_ID` と同じ ID のものがあるか。GitHub の OIDC が対象 repo・Environment・main・workflow に束縛され、`tag:kio-ci` だけを要求できるか。
   - Machines で両ホストのアドレス、最終接続日時、key の有効期限、tag を確かめる。
5. **両ホストの準備状況**
   - Windows の `kio-ci` と制限付き転送、WSL の `kio-test`・forced command・専用 rootless Docker/GPU を両台で確認する。
   - 接続先の WSL が稼働し、空き VRAM が 6,269 MiB 以上あり、Windows・WSL の他ワークロードが GPU を使用していないことを管理者が確認する。受入コントローラは専用 Docker 内の他コンテナ、共有 Docker の稼働中 DeviceRequests 件数、ホスト全体の VRAM を検査する。

初期構築では OIDC/grant、固定 host key、Windows の制限付き forwarding、WSL の forced command と候補未配置時の拒否、既存管理者経路の維持を確かめる。F4 前には選んだホストの候補に束縛された deployment を照合する。GitHub の dispatch 権限を持つ主体も再確認する。main 専用 Environment の branch 制限と、dispatch できる利用者の権限は別に確かめる。整理後の候補 SHA から、移管済みの OCR 設定を含む deployment を生成する。[候補の固定入力と配置の手順](../scripts/v1-local-gpu/README.md#candidate-bound-deployment-proposal) に従う。

確認結果は `scratch/reports/f4/external-status.md` に、9 件を実行できるか、項目番号ごとの状態と根拠、利用者が決めたホスト、必要な対応と担当を記録する。
