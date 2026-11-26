# v1 の正式受入（F4）

環境・構成の整理をすべて統合してから、同じ候補 SHA でネイティブ 42 件、有料 provider 6 件、認証付き GPU 3 件と最終集約を一度だけ実行する。実装完了の判定は [v1 計画](v1-plan.md) §2 を参照する。

## 実行前の条件と順序

1. [受入ホストの構築](../scripts/v1-local-gpu/README.md#受入ホストの構築)に従い、検証済みの初期構築用 Linux helper と共有 GPU 照会だけを root 管理で先に配置する。WSL と Windows を構築し、候補未配置の状態で 2 段 SSH の `deployment path is missing` を確認する。
2. 整理後の `main` の完全な 40 桁 SHA を候補にし、同じ SHA の `ci.yml` の push run が成功したことを確かめ、候補 SHA と run ID を記録する。
3. 下記の読み取り確認で外部資源 9 件の準備状況を確認する。F4 前に両台で同じ候補の dispatcher・固定入力・canonical な deployment を照合して配置する。その後 D-20261121-05 に従って GPU ホストを選び、固定 OCR image を用意して接続を確認する。[候補の固定入力と配置](../scripts/v1-local-gpu/README.md#candidate-bound-deployment-proposal)に従う。台帳・権限・予算の確認と初期化条件は [provider 予算台帳](acceptance/provider-budget.md) に従う。全経路の準備が揃ってから dispatch に進む。GPU の経路は、候補を配置する前に [GPU 受入](../.github/workflows/v1-local-acceptance.yml) を `operation=route-probe` で dispatch し、`tag:kio-ci` の runner から両台の TCP 22 と WSL のホスト鍵の照合が成功し、TCP 2222・18443 と管理 Mac の TCP 22 が拒否されることを確かめる。
4. `main` を ref とし、[ネイティブ受入](../.github/workflows/v1-acceptance.yml) を `candidate_sha` で実行する。42 件と 3 OS の候補 package の成功後に、この run ID を外部 2 workflow の `package_run_id` に渡す。
5. [provider 受入](../.github/workflows/v1-provider-acceptance.yml) は `operation=run`、`campaign_id`、`candidate_sha`、`package_run_id` を使う。[GPU 受入](../.github/workflows/v1-local-acceptance.yml) は `operation=run` と同じ `candidate_sha`・`package_run_id` を使う。台帳の予約は失敗・結果不明も消費として扱う。
6. 3 run の成功後、[最終集約](../.github/workflows/v1-acceptance-verify.yml) を同じ候補で実行する。`candidate_sha`、`ci_run_id`、`native_run_id`、`provider_run_id`、`local_run_id` を渡し、集約成功と 51 件の証跡を確認する。候補 SHA、各 run ID、パッケージと集約の証跡を記録する。

## 外部資源の読み取り確認

利用者と管理画面を確認し、鍵の登録・更新日時・状態だけを記録する。秘密値や認証画面の入力内容は記録しない。再ログイン・多要素認証は利用者が操作する。この段階の権限は閲覧までとし、設定の保存、資格情報の発行、課金・予算の変更、SSH 接続、workflow dispatch・rerun は後続の準備・実行段階で扱う。

### 確認済みの記録と未確認の前提

2026-11-21 に三つの Environment が `main` 専用であることを確認した。これらは当日の記録であり、実行前に再確認する。

| Environment | 確認対象 |
| --- | --- |
| `v1-provider-acceptance` | `GEMINI_API_KEY`、`MISTRAL_API_KEY`、`GEMINI_TEST_BUDGET_USD` と `MISTRAL_TEST_BUDGET_USD`（各 USD 10） |
| `v1-provider-ledger-authority` | `KIO_LEDGER_APP_PRIVATE_KEY`、`KIO_LEDGER_APP_CLIENT_ID`、`KIO_LEDGER_APP_ID`、`KIO_LEDGER_APP_SLUG`、`KIO_LEDGER_INSTALLATION_ID` |
| `v1-local-acceptance` | `KIO_LAB_SSH_PRIVATE_KEY`、`KIO_LAB_JUMP_HOST`、`KIO_LAB_JUMP_USER`、`KIO_LAB_KNOWN_HOSTS`、`KIO_TS_CLIENT_ID`、`KIO_TS_AUDIENCE` |

**GPU ホストは研究室の RTX 4060 と個人デスクトップの RTX 4070。** 両台とも WSL 内の kio-test 専用 rootless Docker Engine と NVIDIA Container Toolkit の CDI を使う。Mac 作業中は 4060、停止中・利用不可なら 4070、4070 で作業中は 4070 を使う。両台の構築・接続確認は実行前に行う。同一の受入鍵と両台の Windows・WSL ホスト鍵を登録し、run 開始前に `KIO_LAB_JUMP_HOST` のみを手動で切り替える。3 OS の実行とリース回収が完了するまで接続先を固定する。[構築・切り替え手順](../scripts/v1-local-gpu/README.md#受入ホストの構築)

### 確かめること

各項目について、**状態**（使える／使えない／不明）と**根拠**（どの画面で何を見たか）を書く。

#### A. 有料 provider（6 件）

1. **Google AI Studio / Google Cloud（Gemini）**
   - Environment の `GEMINI_API_KEY` に当たる鍵が、今も有効なものとして一覧にあるか（作成日 2026-09-08 前後、末尾表示が出るならその一致）。削除・無効化・制限の変更がないか。
   - 鍵の属するプロジェクトで課金が有効か。無料枠のままか。
   - embedding のモデル `gemini-embedding-2` が使えるか。予算・上限の設定（Environment の変数では 1 回の受入あたり USD 10）。
2. **Mistral（La Plateforme の console）**
   - `MISTRAL_API_KEY` に当たる鍵が有効か（作成日 2026-09-08 前後）。
   - workspace の課金・支払い方法が有効か。月の上限・残高。正式受入で指定する OCR モデル（`mistral-ocr-4-1`）と Batch が使える plan か。固定モデルは [受入コード](../crates/kio-eval/src/acceptance_provider.rs) を参照する。
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
