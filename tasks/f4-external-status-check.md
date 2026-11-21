# F4 正式な受入の前に、外部の資源の状態を確かめる（Codex、Chrome 操作）

## 目的

kio の v1 の正式な受入（51 件）のうち、外部の資源を使う 9 件が今も回せる状態かを、利用者と一緒に確かめる。

- 有料 provider の 6 件（`.github/workflows/v1-provider-acceptance.yml`）
- GPU 経由のローカルの 3 件（`.github/workflows/v1-local-acceptance.yml`）

**この作業は状態の確認だけである。** 設定・鍵・予算・policy は何も変えない。変更が必要だと分かったら、その内容を報告に書いて止まる。

## 守ること

- 画面上の操作は読み取りだけにする。次の操作はしない：保存、作成、削除、再生成、rotate、revoke、有効化・無効化、課金・予算・上限の変更、Tailscale の policy の保存、GitHub の設定の変更、workflow の実行（dispatch・rerun）。
- 秘密の値（API key、秘密鍵、token、OAuth の client secret）は、表示・コピー・報告への記載をしない。鍵については、存在すること、名前、末尾数文字の表示（サイトが出す場合のみ）、作成日、最終使用日、状態だけを書く。
- ログインやパスワード、2 段階認証が必要になったら、利用者に操作してもらう。代わりに入力しない。
- 出力は repo の `scratch/reports/f4-external-status.md`（repo の root からの相対パス。`scratch/` は gitignore 済み） の 1 ファイルだけにする。ほかの場所にファイルを作らない。スクリーンショットを残す場合は `scratch/reports/f4-shots/` に置き、秘密の値が写っていないことを確かめる。
- repo のファイルは変更しない。commit・push はしない。
- lab の Windows（`kio-lab-windows`）や WSL に SSH で入らない。そこは利用者の作業として報告に残す。

## すでに分かっていること（2026-11-21、統括が WSL から `gh` などで確認）

### GitHub の Environment（repo `ttokunaga-ja/kio`、すべて main だけに限定）

| Environment | 秘密情報（更新日） | 変数（更新日、値） |
|---|---|---|
| `v1-provider-acceptance` | `GEMINI_API_KEY`（2026-09-08）、`MISTRAL_API_KEY`（2026-09-08） | `GEMINI_TEST_BUDGET_USD`=10、`MISTRAL_TEST_BUDGET_USD`=10（2026-09-08） |
| `v1-provider-ledger-authority` | `KIO_LEDGER_APP_PRIVATE_KEY`（2026-09-27） | `KIO_LEDGER_APP_CLIENT_ID`・`_APP_ID`・`_APP_SLUG`・`_INSTALLATION_ID`（2026-09-27） |
| `v1-local-acceptance` | `KIO_LAB_SSH_PRIVATE_KEY`（2026-10-03） | `KIO_LAB_JUMP_HOST`=100.68.218.37、`KIO_LAB_JUMP_USER`=kio-ci、`KIO_LAB_KNOWN_HOSTS`、`KIO_TS_CLIENT_ID`、`KIO_TS_AUDIENCE`（2026-10-03） |

### tailnet の端末（`tailscale status`、個人デスクトップから）

- `ttokunaga-ja`（100.97.109.109、Windows）：個人デスクトップ（WSL の作業環境）。GPU は RTX 4070。Windows の local user に `kio-ci` は**ない**。WSL に `kio-test` user は**ない**。
- `kio-lab-windows`（100.68.218.37、Windows）：Environment の jump host が指している端末。
- `ttokunaga-ja-macbookpro`（100.102.187.14、macOS）：2026-11-21 からの作業環境。経路の文書の提案では、この Mac から `kio-lab-windows` の TCP 22 への接続を残すことになっている。

経路の文書 `tasks/v1-private-actions-route.md` は 2026-09 時点のもので、lab Windows の `kio-ci` アカウント、SSH の設定、WSL の dispatcher の配置を「未適用」としている。

## 確かめること

各項目について、**状態**（使える／使えない／不明）と**根拠**（どの画面で何を見たか）を書く。

### A. 有料 provider（6 件）

1. **Google AI Studio / Google Cloud（Gemini）**
   - Environment の `GEMINI_API_KEY` に当たる鍵が、今も有効なものとして一覧にあるか（作成日 2026-09-08 前後、末尾表示が出るならその一致）。削除・無効化・制限の変更がないか。
   - 鍵の属するプロジェクトで課金が有効か。無料枠のままか。
   - embedding のモデル `gemini-embedding-2` が使えるか。予算・上限の設定（Environment の変数では 1 回の受入あたり USD 10）。
2. **Mistral（La Plateforme の console）**
   - `MISTRAL_API_KEY` に当たる鍵が有効か（作成日 2026-09-08 前後）。
   - workspace の課金・支払い方法が有効か。月の上限・残高。OCR のモデル（`mistral-ocr-latest`）と Batch が使える plan か。
3. **GitHub App（ledger 用）**
   - GitHub の Settings → Developer settings → GitHub Apps に、`KIO_LEDGER_APP_SLUG` の名前の App があるか。repo `ttokunaga-ja/kio` に install されていて、その installation の権限が Contents の read/write を含み、対象 repo が kio だけか。
   - App の秘密鍵が、2026-09-27 前後に作ったもの 1 つとして残っているか（作り直さない）。

### B. GPU 経由のローカル（3 件）

4. **Tailscale の管理画面**
   - Access controls の policy に `tag:kio-ci` の `tagOwners` と、`tag:kio-ci` → `100.68.218.37` の `tcp:22` の grant があるか。wildcard の grant（`src:["*"], dst:["*"]`）が残っているか。全文を報告に写してよい（秘密は含まれない）。
   - Settings の OAuth clients / Trust credentials（workload identity federation）に、`KIO_TS_CLIENT_ID` と同じ ID のものがあるか。GitHub の OIDC（repo `ttokunaga-ja/kio`、Environment `v1-local-acceptance`）を受け入れる設定か。付けられる tag が `tag:kio-ci` か。
   - Machines で `kio-lab-windows` の最終接続日時、key の有効期限、tag。
5. **利用者に聞くこと**（Chrome では確かめられない）
   - `kio-lab-windows` は今も GPU の host として使う予定か。それとも、個人デスクトップ（`ttokunaga-ja`）に移すのか。
   - lab Windows に `kio-ci` user、`sshd_config` の `Match User kio-ci`、WSL の `kio-test` と dispatcher を設定したことがあるか。

## 報告の形

`scratch/reports/f4-external-status.md` に、次の順で書く。

1. 一行の結論：9 件をこのまま回せるか。回せないなら、止めている項目の番号。
2. 表：項目番号、状態、根拠、必要な対応（誰が、何を変えるか）。
3. 利用者の答え（項目 5）。
4. 変更が必要な設定の一覧。どれも実施していないことを明記する。
