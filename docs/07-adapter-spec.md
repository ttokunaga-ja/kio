# 07 Adapter Spec

Adapter (Prepare / Markdownize / Embedding / Rerank) の trait 契約 + 実行形態 + Markdown incremental プロンプト規約。

> 関連: [03-data-model.md §5](03-data-model.md) (`tool_profile_hash` 計算規約) / [04-pipeline.md §3](04-pipeline.md) (incremental Markdownize) / [06-cli-spec.md §9](06-cli-spec.md) (JSON output / Adapter 境界)

---

# 1. 基本方針

Prepare / Markdownize / Embedding / Rerank は Kio core に含めず、**Adapter に委譲** する。OCR は Markdownize Adapter の **内部能力 (capability)** として扱う。Embedding は Text / Image を分離せず、**単一マルチモーダル Embedding Adapter** に統合する。

```
Kio core:                 Adapter:
  object store              Prepare
  snapshot                  Markdownize (OCR は内部能力)
  restore                   Embedding (multimodal)
  search                    Rerank
```

Adapter の実行設定 (cmd / args / url / 認証情報) は **`.kio/` の共有対象に含めない**。各デバイスの `~/.config/kio/tools.toml` に保存する。`.kio/` は生成済み artifact の provenance と互換性判定に必要な `profile_hash` だけを保持する。

R23 の同梱 runtime が実行できる online target は `mistral_ocr_markdownize` と
`gemini_embedding_2` の built-in 実装に限定する。これらの role では `cmd` / `args` /
`url` による実行先の差し替えを受理せず、宣言された model と built-in target の一致を
起動時と実行直前に検証する。上記の実行設定フィールドは現在の built-in role では受理しない。

認証情報の保存規約:

```text
推奨:
1. 環境変数参照:       auth = "env:GEMINI_API_KEY"

許容 (非推奨):
2. tools.toml 直書き:  auth = "plain:<api_key>"
   - tools.toml と直上のディレクトリは利用者所有・所有者限定とする。
     他の利用者へ権限を与える macOS ACL / Windows DACL も拒否する。
     通常の Unix 設定例はファイル 0600、親ディレクトリ 0700。
   - 条件を満たさない平文 credential は KIO-E-ADAPTER-TOOLS-PRIVATE-001
     (exit 2) で拒否する。既存の permission を自動変更しない。
   - 起動時と credential 使用時に、1 MiB 上限の regular・single-link・
     no-follow 読込みを行う。使用時の宣言が読込み済み設定と変わっていたら拒否する。
     事前の形式判定すらできない unsafe file は通常の bounded-read error になる。
   - TOML 構文エラーには設定本文・credential を含めず、byte offset だけを示す。
     CLI の標準エラーと errors.jsonl のどちらにも入力行を転記しない。

禁止 (既定どおり):
   .kio/ 配下・tool-lock.json・tool_profile_hash の入力への認証情報の混入
```

`tools.schema.json` は `auth` フィールドを `^(env|plain):` にマッチする文字列
として規定する ([06-cli-spec.md §10](06-cli-spec.md))。

---

# 2. 実行形態

Adapter は **提供主体ではなく実行形態と決定性** で分類する。

```
online_api               LLM 等のネットワーク越し API (frontier AI が中心)
                         明示的な network opt-in が必要
offline_api              ローカル LLM / ローカル embedding server
                         loopback HTTP を使い得る。非決定的出力はあり得る
deterministic_library    決定論的ライブラリ (PDF text extraction, parser)
                         同じ入力 + 同じ profile なら同じ出力
```

Kio API の契約は実行形態に依らず同じ。

```
Kio core
  → task descriptor (task_id, adapter_kind, input_hash, allowed scope, network permission)
  → device-local Adapter
  → artifact descriptor (output_hash, status, error_code / error_category)
  → Kio core
```

## 2.1 同梱 deterministic Adapter (ベースライン index)

Kio は `deterministic_library` の Prepare / Markdownize Adapter を同梱する。対象: plain text / Markdown / コード、PDF text layer 抽出。OCR・レイアウト解析・画像理解は行わない。**出力は単純な passthrough ではなく、Normalized Markdown v1 (§5.2.1) への決定的正規化**である — 少なくとも Setext 見出し → ATX 変換・生 HTML block の fenced text 化・改行 / 空白 / fence の正規化を行う (§5.2.1 準拠が受け入れ検査 V5 の通過条件のため、passthrough では通常の Markdown (Setext 等) がオフライン基線 index で全滅する)。

> **PDF text layer 抽出の範囲 (実装フィードバック 2026-07-23)**: 実世界の text-layer PDF
> (TeX / LibreOffice 出力) は FlateDecode 圧縮 content stream + subset font の glyph index +
> font ごとの ToUnicode CMap (TeX Live はさらに `/Type /ObjStm` 内へ辞書を格納) で構成される。
> deterministic Adapter の text layer 抽出はここまでを対象とする: **FlateDecode の有界展開**
> (stream あたり 16 MiB 上限)、`/Type /ObjStm`
> の展開、Page → Resources → Font → ToUnicode の解決、CMap (`bfchar`/`bfrange`) による glyph
> index 復号。1 decode の resource ceiling は object 8,192 個 / materialized object 128 MiB /
> decoded stream 合計 64 MiB / page content 16 MiB / emitted text 32 MiB / materialized CMap
> 65,536 entries・32 MiB / page あたり content reference 8,192 個である。これらの超過と malformed
> object-stream admission は `contract_violation` として拒否し、より弱い fallback へ送らない。
> **確信を持って復号できないファイルは空抽出へ縮退し (fail-empty)、従来どおり
> online OCR ルーティングに乗る** — FlateDecode 以外の filter (DCTDecode 等)・ToUnicode を
> 持たない font の glyph 列・スキャン/画像系は引き続き OCR の領分である。この抽出意味論の
> 変更は deterministic profile の `model_version_pin` 1.0.0 → 1.1.0 として**新
> `tool_profile_hash`** に載せた (§9 / [03-data-model.md §5.1](03-data-model.md) — 既存
> instance 内の prepared_hash drift ではなく新 identity としての基線再構築)。

- online Adapter が未設定または network 未承認のとき、Markdownize タスクは同梱 deterministic Adapter で実行する (タスクを止めない)。Embedding タスクは生成しない (検索は text fallback、[05-runtime.md §1](05-runtime.md))
- この状態を **ベースライン index** と呼ぶ。`init → index --yes → search → open <pointer>` の最低体験ライン ([01-positioning.md §3](01-positioning.md)) はベースライン index のみで成立しなければならない
- online Adapter を承認した後の AI 強化は、別 `tool_profile_hash` の artifact として通常の Markdownize / Embedding タスクで生成する (identity 規約 [03-data-model.md §5](03-data-model.md) のとおり。ベースライン artifact とその Evidence Pointer は不変のまま残る)

---

# 3. ネットワーク送信原則と opt-in (正本)

Kio core は、**明示オプトインなしにネットワーク越し API へファイル内容を送信してはならない**。
本節を network opt-in の正本とし、[06-cli-spec.md §2](06-cli-spec.md) / [10-operations.md §1](10-operations.md) / [01-positioning.md §1.1](01-positioning.md)
は本節を参照する。

```text
default: no network transmission (opt-in 未成立の scope からはオンライン送信しない)
```

opt-in の単位・成立・寿命:

```text
単位: scope と Adapter の完全な paired identity。
  scope 側の active approval reference と device-private active grant の両方が、
  scope/membership、tool_id、execution_mode、tool_profile_hash、destination、
  credential binding、trust binding、operation に一致しなければならない。

成立: `kio adapter approve <tool_id>` または `kio adapter approve --all` が、
  確認後に paired record を publish する。config の `allow_network = true`、
  scope reference、device record、または pending record の一つだけでは開かない。
  `approval_pending` は resume に使う中断記録であり、自動 publish されない。

寿命と revoke: identity が変われば grant は一致しない。`adapter revoke` は対象の
  scope approval/pending と device grant を revoke し、`--all` は configured Adapter
  全体を対象にする。送信済み bytes は取り消せない。online_api では
  `allow_network = false` も閉鎖条件である。

送信境界: admission と dispatch の双方で current paired identity を検査する。
  active record の欠落、不一致、revocation、または必要な online_api policy の拒否は
  fail closed である。`adapter status` は scope/device/pending と effective match を
  read-only で表示する。詳細な CLI 構文は [06-cli-spec.md §1](06-cli-spec.md) と
  [14-v1-runtime-contracts.md](14-v1-runtime-contracts.md) に従う。
```

`--online` は temporary opt-in でも grant 作成手段でもない。`index --online` は、
current scope の configured Adapter の少なくとも一つに active external-send paired grant
があることを preflight で要求する。Gemini-only の grant でもこの preflight を満たし得るが、
各 Adapter の dispatch は依然としてその Adapter 自身の exact grant を要求する。検索の
query embedding も current exact paired grant を要求する。`--online` は revoke、identity
不一致、または policy 拒否を修復しない。

`--offline` は当該 invocation の**全 HTTP dispatch**を禁止する。online provider だけでなく、
認証済み loopback local Adapter、provider の poll、output download、upload cleanup も対象である。
未実行の task はこの invocation で送信せず、HTTP 回復も行わない。適用対象は index、batch
resume/retry、reindex、repair rebuild-db、search、および HTTP Adapter を使い得る同等の経路である。

**01-positioning.md との整合**: デフォルト同梱 Adapter は online_api (frontier AI) だが、
paired grant が成立するまで送信は始まらない。
"frontier AI default" は同梱・推奨構成を指し、"default: no network transmission" は
opt-in 未成立状態の既定値を指す。両者は矛盾せず、明示承認フローが接続する。

オンライン API Adapter を使う場合、ユーザーがどの scope / file / task を送信対象にしたかを
記録する。オフライン API / 決定論的ライブラリの場合も `execution_mode` と `profile_hash` は
記録する。

> **local HTTP Adapter の loopback 制限と paired grant (2026-09-08)**:
> `execution_mode = "offline_api"` でも別 process への HTTP dispatch である。loopback
> 制限は宛先制限であって consent/approval の免除ではない。local Adapter には current
> scope/device paired network grant と、認証を使う場合には device-local managed CA trust が
> 必要である。`--offline` はこの HTTP dispatch も禁止する。
>
> **(1) url は loopback リテラルに限定する。**
> 受理するのは `127.0.0.1` / `localhost` / `[::1]` / UNIX domain socket **のみ**。
> **判定はリテラル文字列に対して行い、ホスト名解決の結果を見ない** (解決結果を信用すると
> DNS rebinding で loopback を名乗る外部ホストへ送信できてしまう)。違反は
> `KIO-E-CONFIG-OFFLINE-URL-001` (exit 2、[06-cli-spec.md §8](06-cli-spec.md) /
> [10-operations.md §11.1](10-operations.md))。検証点は tool-lock materialize /
> adapter 登録時 — §2 が `offline_api` を「ネット送信なし」と**定義**している以上、
> 定義に反する構成は実行前に拒否する。
>
> 判定の細部 (2026-07-26 実装時に確定):
>
> - **スキームは必須** (`http://` / `https://` / `unix:`)。`127.0.0.1:8000` のような
>   スキームなしの形は authority の切り出しが一意に定まらないため受理しない
> - **userinfo (`@`) を含む url は拒否する。** `http://127.0.0.1@evil.example/` の
>   実ホストは `evil.example` であり、`@` の前は認証情報である。解析して捨てるのではなく
>   存在自体を拒否する — 解析の取り違えが起こり得ない形にするため
> - ホストは上記 3 リテラルとの**完全一致**。`localhost.evil.example` /
>   `127.0.0.1.evil.example` のような接尾辞付きは拒否する
> - ポートとパスは自由 (`http://localhost:8000/v1`)
>
> **検証は 2 箇所で行う。** config 読込時 (`tools.toml` の検証) と
> **実行直前の宣言再検査**の両方。後者を欠くと、起動時検証を通った宣言が
> 実行の瞬間に別経路で拒否される (あるいはその逆に素通りする)。
>
> `kind = "offline_api"` が外部 URL を受理することはない。加えて、loopback endpoint
> への本文送信は paired grant がなければ拒否する。scope config の一行だけ、device grant
> だけ、または trust snapshot だけでは送信を許可しない。
>
> **現行実装 (2026-08-02 更新)**: Embedding と Markdownize の**両方**で `url` が
> 上記の条件下で**受理される** (Markdownize は 2026-08-02 に解禁。それ以前は
> 一律 schema error だった)。**両者の `cmd` / `args` は引き続き一律 schema error**
> 。
>
> 緩和は **role 単位ではなく target 単位**である。`offline_api` を宣言できるのは
> その kind を持つ tool_id だけで、cloud API の tool_id に `offline_api` を
> 宣言することも、その逆も schema error になる。
>
> ```toml
> [embedding.qwen3_vl_embedding_local]
> kind  = "offline_api"
> url   = "http://127.0.0.1:8000"      # loopback リテラルのみ。末尾の /v1 は不要
> model = "Qwen/Qwen3-VL-Embedding-2B" # 省略時はこの値
>
> [markdown.paddleocr_vl_local]
> kind  = "offline_api"
> url   = "http://127.0.0.1:8080"      # 末尾の /layout-parsing は不要
> model = "PaddleOCR-VL-0.9B"          # 接頭辞 `PaddleOCR-VL` で照合
> ```
>
> > **Markdownize の口は `POST {url}/layout-parsing` であって
> > `/v1/chat/completions` ではない。** PaddleOCR-VL は 2 段構成で、
> > OpenAI 互換の `genai_server` が提供するのは段 2 (VLM 認識) だけであり、
> > bbox もレイアウトも読み順も返らない。上流も OpenAI クライアントでの
> > 直接利用を明示的に非推奨としている。詳細は
> > `tasks/local-adapter-plan.md` §11 の V2。
>
> **secrets hold は offline_api にも適用する (2026-08-02 確定)。**
> Tier A ([10 §1.1](10-operations.md)) として明示承認で取り込まれたファイルは、
> `--send-secrets` が無い限り**ローカル Adapter にも渡さない**。paired network grant と
> secret-send grant は別であり、secrets hold が問うているのは**中身が資格情報か**
> であり、送り先のネットワーク上の位置ではない。ローカルモデルサーバは
> 別プロセス (多くはコンテナ) であり、プロンプトをログに残しうる。
> **誤る向きが非対称である**ことが決め手で、緩い側で誤れば資格情報が
> モデルサーバのログに落ちるが、厳しい側で誤ってもユーザーがフラグを
> 1 つ足すだけで済む。
>
> **`tool_id` はテーブル名で表す。`[embedding]` の直下に `tool_id` は書けない**
> (2026-07-28 訂正: 以前の例はそう書いていたが `KIO-E-CONFIG-SCHEMA-001` で
> 弾かれる)。flat な `[embedding]` 形も使えるが、その形は tool_id を持てないため
> **`kind` で target を解決する** — offline embedding 実装が 2 つ目になるまでは
> 一意に定まる。
>
> `auth` を省略しても paired grant は必要である。認証する local peer は、`adapter trust`
> lifecycle が管理する CA snapshot だけを信頼する。legacy `ca_pem_path` は拒否する。Adapter
> は `POST {url}/v1/embeddings` に §5.3 (2) の `messages` 形式で 1 item ずつ送る。

---

# 4. 共通メタデータ

すべての Adapter は次を返す:

```
AdapterProfile:
  adapter_kind          "prepare" | "markdownize" | "embedding" | ...
  adapter_id
  execution_mode        "online_api" | "offline_api" | "deterministic_library"
  tool_profile_hash     計算規約は 03-data-model.md §5.1
  version
  capability_flags      ["ocr", "layout_detection", "incremental_update", ...]
  billable_kinds        billable を宣言する Adapter (§5.5 条件 6) は必須 — 報告し得る
                        `billable_units.kind` の閉集合 (拒否課金の有無の宣言 = 下記 `reject_billing`)。
                        **実行時 usage の kind が宣言集合外の場合は課金 field の不良として
                        estimated 縮退 + warning ([04-pipeline.md §5.4](04-pipeline.md) — 応答の
                        受否・outcome・`contract_violation_count` は変えない)**。宣言集合の執行面は
                        送信前の pricing 被覆検査の入力 ([10-operations.md §11.3](10-operations.md))
  reject_billing        billable を宣言する Adapter (§5.5 条件 6) は必須 — "billable" | "nonbillable"
                        の閉 enum (投入拒否 (permanent 4xx) に課金する provider か否かの機械可読宣言。
                        billable_kinds と同じく出力非影響 = tool_profile_hash 非対象。欠落・未知値
                        は fail-closed = "billable" として扱う)。usage 欠落の permanent 4xx を
                        「正当な非課金 reject (確定額 0)」と「billable provider の欠落 (estimated
                        縮退)」に分離する判定源 ([04-pipeline.md §5.4](04-pipeline.md))
  allow_network

AdapterRun:
  task_id
  input_hashes
  output_hashes
  status                "pending" | "running" | "done" | "partial" | "failed"
                        (partial = unit 単位の部分失敗, 04-pipeline.md §5.2。`full_required`
                         (= fallback_to_full) は request の制御結果であり、AdapterRun は "done" でも
                         normalized task の完了を意味しない。送信前なら Kio が Full を選べるが、有料
                         incremental の送信後に自動 Full 再送はしない。後者は result_unknown として
                         明示 `--resend-unknown` 承認を待つ。request outcome の区別は cost_ledger 側
                         ([04-pipeline.md §3.2](04-pipeline.md)))
  error_code            機械判定用 (06-cli-spec.md §8)
  error_category        transient | permanent | rate_limit — 04 §5.3 の retry 分類の入力
                        (集計用の粗分類 — auth / quota / invalid_input 等の細分は error_code が
                         担い、retry 対応は 04 §5.3 の表が error_code 基準で優先する)
  retry_after_ms        optional — provider の Retry-After を透過 (rate_limit 時)
  usage                 one-of { usd } | { billable_units } — request 単位の課金報告 (§5.5)。
                        usd = 有限・非負の実測額 (billable reject では provider が宣言する請求額を
                        これに充てる — §5.5 条件 6。第三の field は設けない)。billable_units =
                        **unique-kind の配列** `[{ kind, count }, ...]` (1 要素以上。kind = "pages" |
                        "tokens_in" | "tokens_out" の閉 enum — 拡張は spec 改訂。count = 非負整数。
                        **kind の重複は課金 field の不良 (estimated 縮退 + warning —
                        [04-pipeline.md §5.4](04-pipeline.md))**。USD 換算は要素ごとの単価 × count の**合算** —
                        input/output token 両課金の provider を単一報告で表現する)。単価解決元 = tools.toml の
                        `[pricing]` 単価表 (kind → USD 単価 — [03-data-model.md §11](03-data-model.md)、
                        **単価の正本は tools.toml** — tool-lock ではない。schema 型と billable Adapter の
                        kind 被覆必須は [10-operations.md §11.3](10-operations.md)、kind の単価が解決
                        不能な場合の終端は estimated 縮退 — [04-pipeline.md §5.4](04-pipeline.md))。**換算は終端 Tx
                        時点の表で確定し、以後の単価変更で再計算しない** (台帳 UPDATE 禁止と整合)。
                        **billable な terminal 応答 (成功・billable reject・fetch_output・sync 応答・
                        正常な制御応答 (fallback_to_full — [04-pipeline.md §3.2](04-pipeline.md)))
                        で必須** — 欠落・不正値は estimated 記帳へ縮退する ([04-pipeline.md §5.4](04-pipeline.md)
                        の記帳値事前検証と同一規範 — **応答の受否・outcome・violation 予算は変えない**)
```

---

# 5. 各 Adapter の trait

## 5.1 Prepare

```
input:
  raw_hash, media_type
output:
  prepared_object_hashes
  prepared_unit_hashes        (page / slide / sheet / image 単位)
  image_object_hashes         (画像抽出があれば)
metadata:
  unit_kind, page_number, mime, fingerprint (semantic_fingerprint)
```

PDF page image、Office intermediate、抽出済み image など、後続 Markdownize / Embedding が扱いやすい単位を作る。

**Office intermediate の変換機構 (DOCX / PPTX — 実装フィードバック 2026-07-23)**: DOCX / PPTX の
unit 化 ([04-pipeline.md §2](04-pipeline.md) の表) は外部 renderer (LibreOffice headless
`soffice --convert-to pdf`。解決順 = `KIO_OFFICE_CONVERTER` env → PATH 上の `soffice`) による
**変換 PDF 経由**とする。DOCX は変換 PDF の page を `page:N`、PPTX は 1 page = 1 slide の対応で
`slide:N` として列挙する。**変換出力は決定論化する** — PDF の `/CreationDate` `/ModDate` `/ID` 等の
揮発メタデータを同長固定値へ正規化してから hash する (xref offset を壊さない)。これにより同一
renderer 版内で prepared_hash が安定し、renderer 更新による出力変化は
[03-data-model.md §2.1](03-data-model.md) の「prepare profile / renderer 変更による prepared_hash
変化が駆動する再 Markdownize」(gen+1 の第二の合法経路 — 確認プロンプト + budget guardrail) が
そのまま吸収する。renderer の名称・版は provenance として記録し、hash 入力にはしない (同一 bytes =
同一 identity)。**renderer が環境に存在しない場合、当該 Office ファイルの online タスクは enqueue
しない** (doomed task を作らない) — `index_status` ([05-runtime.md §1.7](05-runtime.md)) に理由付きで
可視化する。実行時の変換失敗は contract_violation ([04-pipeline.md §5.3](04-pipeline.md) — 同一入力の
再試行 1 回) に合流する。音声の変換機構は現在の契約に含めない。

macOS の LibreOffice は、Document Foundation の署名を検証できる bundle と
26.8 以降の release を必要とする。公式 26.2.5 は `--version` が成功しても、
headless backend を含まないため、実変換時に Cocoa の初期化で隔離内から異常終了する。
対応版未満・不正な version 応答は変換前に拒否する。この版の条件は対応方針であり、
将来の配布物がすべて動作するという保証ではない。各固定配布物について実際の
DOCX/PPTX 変換と隔離の受入試験を行う。v1 の 3 OS Actions は公式 26.8.0 を
配布物ごとの SHA-256 で固定する。bundle 内の変更や隔離の緩和で旧版を動かさない。

Linux の TDF 公式 DEB では、厳密な `/opt/libreoffice<major>.<minor>/program/soffice`
配置を認識し、`program`、`share` と初回の専用 profile 作成に必要な `presets` を
読み取り専用で許可する。各 leaf とその祖先は root 所有・group/other 書込み不可の
実ディレクトリであることを検証し、symlink 経由の配置を拒否する。必要な leaf が
欠ける場合は拒否し、実行ファイルの指定だけで任意の sibling を読み取り可能にしない。

**Office package preflight (DOCX / PPTX)**: LibreOffice に渡す前に raw ZIP を最大 100 MiB、central-directory を最大 4,096 entry、各 member を最大 32 MiB、全 member の declared aggregate を最大 256 MiB として検証する。必要な `[Content_Types].xml`、`_rels/.rels`、main XML の読取り合計は 6 MiB に制限し、CRC を検証して読む。encrypted / split ZIP、ZIP64、重複・非 UTF-8・unsafe part name、非 Stored/Deflated member は拒否する。XML は UTF-8、nesting 256、attribute 1,024 を上限とし、DTD/entity declaration を受けない。DOCX / PPTX ごとに genuine な MIME override、内部 root relationship、main part と namespace を検証する。preflight failure は renderer を起動せず contract violation とする。

**Linux renderer の資源境界 (v1 必須契約)**: 変換ごとに systemd user scope と cgroup v2 を
使用し、本文を処理する前に実際の所属と制限値を確認する。CPU / memory / pids controller、
`cgroup.kill`、安全な descriptor 相対解決、利用者の systemd manager が必須であり、
不足時は変換を拒否する。製品が sudo を呼び出したり、ホストの権限設定を変更したりしない。
native Actions の受入対象は Ubuntu 24.04 / systemd 255 である。

既定値は invocation 全体で memory 2 GiB、swap 0、task 64、CPU bandwidth 1 CPU とする。
task 数には launcher、監視用の待機プロセス、renderer の全 thread を含む。
CPU 時間は `cpu.stat` の合計を定期確認する。時間の上限判定には監視間隔と quota period による
超過があり、瞬時の停止を保証する値ではない。memory / task 上限はカーネルで適用する。
OOM は scope 全体を終了させ、task 上限への抵触や資源情報の読取り失敗も変換失敗とする。
終了と同時にカーネルの accounting が削除された場合は、資源情報を取得できない失敗として
返す。取得済みの OOM counter がない限り、プロセスの終了だけから OOM と断定しない。
複数の独立した invocation に対するマシン全体の合計上限を表す値ではない。

起動準備と変換は同じ wall-clock deadline に含め、後始末には最大 5 秒の追加猶予を設ける。
成功時も残存する scope 内プロセスを終了し、空になったことを確認してから出力を受け入れる。
確認できない場合は成功を返さない。親終了時の停止に加え、systemd の有限 runtime と
明示した全プロセスへの SIGKILL を独立した終了手段とする。renderer には user bus、
host cgroup / procfs、制御 descriptor を渡さず、既存の filesystem / network 隔離を保つ。
この契約の実装・試験状況は [v1 実装記録](../tasks/v1-implementation-progress.md) に記録し、
前提検査の成功だけで実変換や native Actions の合格とは扱わない。

**XLSX の unit 化 (実装フィードバック 2026-07-25 — 上の「対象外」を解除)**: XLSX は
**変換 PDF を経由しない**。DOCX / PPTX が変換 PDF に載るのは page と slide が**それ自体で視覚的な
unit** だからであり、sheet にはそれが無い。実測: 10 列 1 シートを `soffice` で PDF 化すると
**2 ページに縦割り**され、ヘッダ `作業区分` が 1 ページ目・その値が 2 ページ目へ分かれ、
**同一行の左右半分を再結合する手がかりが残らない**。行の同一性が壊れた unit は evidence pointer の
指し先として誤りであり、OCR を通せばさらに劣化する。加えて cell は**既に構造化テキスト**であり、
画素へ落として読み戻すのは失われていない構造を壊す往復である。

したがって XLSX は**ローカルで決定論的に直接抽出する** (`kio_adapter::xlsx_extract`)。受理上限は
workbook あたり 256 sheets・1,000,000 cells（疎な列位置の padding を含む）・セル文字列合計 16 MiB・
出力 Markdown 合計 16 MiB、sheet あたり 50,000 rows・1,024 columns、ZIP central-directory 4,096 entries、
任意の inflated ZIP member 64 MiB とする。self-closing row/cell にも同じ上限を適用し、
共有文字列の複写・列 padding・Markdown escape より前に残量を検査する。raw ZIP は 100 MiB、aggregate read は 256 MiB、XML は nesting 256 / attribute 1,024 を上限とする。central/local header の name、size、CRC は一致を確認し、CRC-checked read を行う。encrypted / split ZIP、ZIP64、重複・非 UTF-8・unsafe part name、非 Stored/Deflated member、DTD/entity declaration、malformed XML は拒否する。超過・不正 ZIP/XML は部分抽出や空成功にせず
`KIO-E-PREPARE-XLSX-EXTRACT-001` の contract violation とする。

- **unit = worksheet 1 枚**。`unit_key` は 04 §2 / QB27 の `sheet:` 規則 —
  NFC 正規化 → 元名の `#` を `##` へ escape → **その後で**同名 2 枚目以降に `#2` / `#3` を付す
  (順序が逆だと実名 `A#2` が「`A` の 2 枚目」と衝突する)。
- `prepared_hash` / `fingerprint` は**抽出後 Markdown のバイト列**から取る (PDF 分岐が page text から
  取るのと同じ形)。1 シートの編集はそのシートだけを再 prepare する。
- **数値書式は装飾ではない。** `0%` 書式の下の格納値 `0.5` は **50%** を意味し、日付は serial 日数
  として格納される。生値を索引に入れることは体裁の問題ではなく**検索を汚す誤答**なので、
  `numFmt` (ECMA-376 §18.8.30 の組み込み ID + `styles.xml` のカスタム書式) を適用してから索引化する。
  dogfood corpus は 20/20 ファイルがカスタム `numFmt` を宣言していた。
- **読めない workbook は空成功にしない** — `KIO-E-PREPARE-XLSX-EXTRACT-001` で落とす。空を返すと
  「存在するが内容なし」として索引に載り、繰延中の XLSX が消えていたのと同じ形になる。
- **sheet に埋め込まれた chart / 画像は本追記の対象外。** これは真に視覚的で直接抽出では読めないが、
  「file X の sheet M の中の image N」という evidence 上の同一性が未定義であり、
  `image_object_hashes` は 3 つの構造体に**宣言があるだけで書き手も読み手も無い**。
  半端に配線せず、抽出器が**件数を報告して穴を可視に保つ** (`XlsxDocument::media_paths`)。

## 5.2 Markdownize

OCR は独立 Adapter ではなく **本 Adapter の capability** として表現する。

```
input:
  raw_hash, media_type
  prepared_unit_hint          (optional)
  mode                        "full" | "incremental"
  previous (incremental 時のみ): { raw, normalized_units, tool_profile_hash }
  hints (incremental 時のみ):   { changed_unit_keys, added_unit_keys, removed_unit_keys, page_fingerprints }
  tool_profile_hash
  spec_version
output:
  mode_used                    "full" | "incremental"
  updated_units / added_units / removed_unit_keys / unchanged_unit_keys
  # updated / added の各 unit は owned_image_hashes を必須とする。
  # 実際に取得・検証した画像バイトの hash だけを格納し、画像なしは []。
  # Markdown の URI や provider の自由記述 metadata から所有関係を推測しない。
  failed_units                 [{ unit_key, error_kind }] — 部分失敗の unit (error_kind は
                               [04-pipeline.md §5.3](04-pipeline.md) の閉 enum。04 §3.2 V1 の被覆に
                               含める (full は V6 の被覆)。persist されず manifest 側で failed へ遷移
                               — partial の表現手段)
  fallback_to_full             bool
  reason
  # Evidence Pointer は Adapter output に含めない — 必須フィールド (chunk_hash / commit) は
  # chunking と snapshot の後にしか存在しないため、発行は Kio core が行う (08 §2.1)
capability_flags:
  ocr, layout_detection, table_extraction, speech_to_text, incremental_update
```

incremental の詳細プロンプト規約は §8 (生成 LLM 系のみ。§8 冒頭の適用範囲を参照)。

**標準 Adapter (非 text-native)**: PDF / DOCX / PPTX / 画像の Markdownize 第一候補は Mistral OCR 系文書処理 API (`mistral_ocr_markdownize`) とする (経緯: 旧 `research/markdown.md` — git 履歴)。規約:

- 表は Markdown 本文に inline で保持する (`table_format=null` 相当)。独立 table object は作らない。
- 文書内 embedded image は抽出して image object ([03-data-model.md §2](03-data-model.md)) として保存し、Markdown 内の参照は `kio://<scope_id>/object/image/<image_hash>` に置換する ([08-evidence-pointer-spec.md §2.3](08-evidence-pointer-spec.md))。実装は Step 2 ([09-mvp-scope.md §3.1](09-mvp-scope.md))。
- bbox / page / confidence score は unit metadata に記録する。**Evidence Pointer schema には含めない**。
- **図中テキストの検索可能性 — bbox_annotation を採用 (2026-07-04 実 API 境界調査で確定)**: 実測
  (`experiments/ocr-verification`、段階 fixture C0-C5 + 曖昧画像 15 枚) により、表は複雑・ラスタ化でも
  100% textize される一方、**チャート/グラフ内テキスト (軸ラベル・凡例・値) は 100% images[] へ消失**
  し (C3 が境界)、ホワイトボード写真風は ~55%、フロー図入りスライドは ~41% を失うことを確認。
  対策として Mistral の **bbox_annotation (+25% コスト) を既定 ON** とし、images[] として返る領域の
  説明+書き起こしを取得して unit metadata に記録し、chunk 化時に image 参照近傍へ検索可能テキスト
  として取り込む (`.kio/config.toml` の `[markdownize] bbox_annotation = true` (既定) で制御 — folder-config schema の正式 key ([10-operations.md §11.3](10-operations.md))。**値は出力に影響するため tool_profile_hash に畳み込む** = 切替は世代判定に乗る)。Markdownize は文書 1 版につき 1 回のコストであり
  incremental 再利用でさらに希釈されるため +25% は budget 内。生成 LLM (Gemini Vision) による二次
  追加の生成 LLM fallback は現在の契約に含めない。
  - wire は次の exact `bbox_annotation_format` JSON Schema 1 個を使い、説明/書き起こし指示は schema
    field description に固定する (bbox 専用 prompt parameter は使わない)。各 `pages[].images[]` の
    `image_annotation` は `short_description` / `transcribed_text` の厳密 JSON とする。JCS byte 列の
    sha256 `sha256:79443071238e373b41e34818e91d52ea69d2bba70a30169cda2cf98bfd8bea76`
    を enabled profile の `prompt_template_hash` とする
    (2026-07-27 訂正: 従前ここに書かれていた `sha256:9404f8ff…` は下記 JSON のどの版
    — 外側込み / `json_schema` のみ / `schema` のみ — の JCS とも一致せず、由来を再現できない。
    正しい値は `bbox_annotation.rs` の `BBOX_ANNOTATION_FORMAT_HASH` および
    同ファイルの凍結 JCS byte 列と一致し、下記 JSON から再現できる。
    §5.3 の D3 が「§5.2 と同型」と述べる参照実装がここなので、誤った定数は
    ローカル embedding profile の算出まで波及する)

    ```json
    {
      "type": "json_schema",
      "json_schema": {
        "name": "kio_bbox_annotation_v1",
        "strict": true,
        "schema": {
          "type": "object",
          "additionalProperties": false,
          "properties": {
            "short_description": {
              "type": "string",
              "description": "Describe the figure briefly in plain text. Do not use Markdown or HTML."
            },
            "transcribed_text": {
              "type": "string",
              "description": "Transcribe all visible text verbatim in plain text. Do not use Markdown or HTML."
            }
          },
          "required": ["short_description", "transcribed_text"]
        }
      }
    }
    ```

  - annotation text は newline→LF / NFC / non-newline-control 除去後、provider 由来の各行を次の exact
    CommonMark source escape に通す: original `&`→`&amp;`、`<`→`&lt;`、`>`→`&gt;`、それ以外の
    ASCII punctuation (`U+0021..002F`, `U+003A..0040`, `U+005B..0060`, `U+007B..007E`) は文字の前に
    `\` を 1 個付け、その他は変更しない。変換後の各行を trusted prefix
    `> Kio figure description: ` / `> Kio figure text: ` の後へ置き、対応 image URI 直後の persisted
    unit Markdown に入れる。unit metadata にも同じ post-escape strings を保持し、検索 bytes と Evidence
    span の元を一致させる。この変換後の CommonMark AST は provider 由来の link / image / raw HTML /
    autolink node を 1 件も含んではならない
  - 上限は image 256/page・4,096/response、description 4 KiB、transcription 64 KiB、aggregate
    16 MiB、bbox coordinate 0..=1e9 かつ positive area。string/aggregate byte 上限は untrusted response
    decode 時と上記 canonical escape 後の両方で検査し、膨張後も超過を許さない。annotation
    policy/profile は task identity に含め、既存 annotation 無し Done task を default-on の完了扱いにしない
- 生成 LLM (Gemini / Claude / GPT 等) は Markdownize の主処理ではなく、OCR 後の品質検証・図表解釈に使う。

> **実地検証済み (2026-07-03、設計宿題 #6 解消 [09-mvp-scope.md §5.4](09-mvp-scope.md))**: 合成 fixture (複雑表・日本語・数式・埋め込み画像、4 ページ) を sync / Batch 両モードで検証: 表セル一致率 1.0 (17/17)、日本語 CER 0.0、画像抽出 1/1 (placeholder 形式も §5.2 想定どおり)、数式は LaTeX でテキスト化。単価は公称一致 (API $4 / Batch $2 per 1,000 pages)、Batch のジョブ往復は 4 ページで約 24 秒。ハーネスと実測ログは `experiments/ocr-verification`。

## 5.2.1 Normalized Markdown 形式 (最小凍結 — 2026-07-18)

全 Markdownize Adapter の出力 (normalized unit の Markdown) は次の最小形式に従う。chunk span と
Evidence Pointer のバイト位置は保存された bytes に対して定義されるため、**この形式は実装後に
変えると互換性コストが高い** ([10-operations.md §11.5](10-operations.md)) — 以下を v1 として凍結する:

- **エンコーディング**: UTF-8 (BOM 禁止)、Unicode 正規化 NFC、改行 LF のみ、行末 trailing space 禁止、
  ファイル終端は LF 1 個
- **見出し**: ATX (`#`〜`######`) のみ (Setext 禁止)。chunk 境界規則 ([04-pipeline.md §4.1](04-pipeline.md))
  の入力
- **表**: GFM table 記法で inline 保持 (§5.2 規約と同じ — 独立 table object は作らない)
  - **provider が見出し行を名指ししない場合、先頭行を見出しへ昇格させない。**GFM は
    見出しの無い表を書けないので**空の見出し行** (`| | |`) を置く。実測した 3 つの表のうち
    2 つは先頭行が見出しで 1 つは本文であり、応答にはこれを区別する field が無い —
    昇格させると本文の行を列名として §9 で永久に凍結することになる
- **画像参照**: `![...](kio://<scope_id>/object/image/<image_hash>)` のみ
  ([08-evidence-pointer-spec.md §2.3](08-evidence-pointer-spec.md))
- **生 HTML / autolink**: 禁止。**provider 由来の生テキストを Markdown 本文へ埋め込む場合は、由来を
  問わず §5.2 bbox_annotation と同じ CommonMark source escape を適用する** (`&` `<` `>` の実体参照化 +
  ASCII punctuation の `\` 前置 — bbox 専用の手順ではなく全 Markdownize 出力共通の規約)
- **code fence**: CommonMark の ``` fence (チルダ不可)。**fence 内の「無変換」は構文的変換 (escape /
  参照置換) の禁止のみを意味する — エンコーディング (UTF-8/NFC)・改行 (LF)・trailing space 禁止は
  fence 内にも適用する** (適用しないと CRLF/NFD を含むコードで v1 の byte 決定性が壊れる)
- 上記の準拠は Kio 側受け入れ検査 ([04-pipeline.md §3.2](04-pipeline.md)) の構造検証に含める

media 別の変換規約 (何を見出しにするか等) は Adapter 実装の裁量 (tool_profile_hash が識別する)。
本節が固定するのは**バイト表現の規約のみ**である。

## 5.3 Embedding (multimodal)

```
input_type:           "text" | "image" | "markdown_chunk" | "image_object" | "query"
input:
  items: [{ id, text|path, mime? }]
output:
  vectors: [{ id, vector }]
  dimensions, distance, modality
metadata:
  adapter_id, model_family, version, embedding_profile_hash
```

**Embedding 応答の受入検査** (markdownize の V1〜V6 に相当する): (1) `vectors[].id` は入力 id 集合と**全単射** (欠落・過剰・重複は違反)、(2) `dimensions` は profile と一致し全 vector が同次元、(3) 全要素が**有限値** (NaN/Inf 拒否) かつ**非ゼロ vector**、(4) float32 への決定的変換と **L2 正規化は core 側で実施**する (Adapter の正規化有無に依存しない)。**変換・正規化後の最終 vector にも (3) と同じ有限・非ゼロ (かつ単位ノルム — 許容誤差内) を再検査する** (underflow の零 vector / overflow の Inf を index に入れない — 違反は同じ contract violation)、(5) 応答 metadata の `embedding_profile_hash`・`modality`・`distance` が期待 profile と一致する (同次元の別 vector space の混入を契約で拒否する — 不一致は同じ contract violation)。違反応答は全体 reject — contract violation として課金・再試行は §5.8 相 3 と同じ規則に従う ([04-pipeline.md §5.8](04-pipeline.md)。再試行分類は [04-pipeline.md §5.3](04-pipeline.md))。`failed_units` 相当の部分失敗 field を持たない **all-or-nothing 契約**である。

Batch 応答の契約違反には、本節の「契約違反後の新規 Batch 停止」を優先して適用する。
共通の再試行予算が残っていても、未解除の違反がある scope / embedding adapter の新規 Batch は送信しない。

Text Embedding Adapter / Image Embedding Adapter は**採用しない**。同一 Embedding Adapter が同一 profile で多モダリティを単一 vector space へ写像する。

> **実地検証済み — 単一 multimodal profile を採用 (2026-07-03 再検証で確定)**: 初回調査は「Gemini Embedding 2 multimodal は preview で pin 不可」を根拠に text-only 緩和を適用したが、事実誤認 (`gemini-embedding-2` は 2026-04-22 に GA、pinned stable 版あり) が判明し**撤回**。再検証 (`tasks/step3-embedding-verify.md` の再検証節) により本節冒頭の本来の契約どおり **単一マルチモーダル Embedding Adapter** を採用する。確定 profile: **`gemini-embedding-2` (GA 版を Adapter が起動時解決して pin、§6) / 768 次元 (MRL 切り詰め — 切り詰め後次元も profile に固定) / cosine / `modality="multimodal"` / `mode="online"`** (Vertex はバッチ推論非対応のため sync 呼出 — client 側の並列は**タスク間** (別 batch_requests 行) で行い、単一タスク内の複数 request は直列 ([04-pipeline.md §5.4](04-pipeline.md) の縮退 2 相)。429 は rate_limit 分類で backoff — §5.5)。current enrichment は text chunk と chunk から参照される image object を同じ vector space に埋め込む。audio は現在の入力種別に含めない。text 品質は MTEB で前世代 text 専用モデルを上回り日本語も同格 (再検証節)。コスト: 10 万 chunk 初回 ≈ $10 (単月 budget 内)。**非 multimodal の embedding profile (`modality="text"` 等、別ベクトル空間への埋め込み) は採用不可** — tool-lock materialize / adapter 登録時に `KIO-E-EMBED-MODALITY-001` (exit 2) で拒否する ([03-data-model.md §7](03-data-model.md))。

> **訂正 — Embedding にも Batch レーンがある (2026-07-24)**: 直前の段落が採用理由に挙げる
> 「Vertex はバッチ推論非対応のため sync 呼出」は、**根拠と実装先が食い違っていた**。
> 実装のエンドポイントは Vertex AI ではなく **Gemini Developer API**
> (`https://generativelanguage.googleapis.com/v1beta`) であり、そちらは
> **embedding の Batch を提供している** ([Batch API](https://ai.google.dev/api/batch-api) /
> [発表](https://developers.googleblog.com/en/gemini-batch-api-now-supports-embeddings-and-openai-compatibility/))。
> Vertex AI Batch Prediction が `gemini-embedding-001` を除外している事実と混線したものである。
> 単価は **標準 $0.20 / 1M tokens・Batch $0.10 / 1M tokens (50% off)** (text、`gemini-embedding-2`)。
>
> したがって Embedding Adapter も **`preferred_request_kind` を持つ** (§5.5 のレーン選択子を
> `MarkdownizeAdapter` 専用にしない)。Embedding の Batch レーンは次の形をとる。
>
> - 投入: `POST {base}/v1beta/models/{model}:asyncBatchEmbedContent`。
>   入力は **inline** (`batch.inputConfig.requests.requests[]`、各要素が `request` (EmbedContentRequest) +
>   `metadata`) を用い、**File API へのアップロードは行わない**。
> - **したがって相 2a (upload) が存在しない。** `upload_id` は恒久 NULL、`provider_scope_id` は
>   **job 作成の直前**に記録する (相 2a 着手の印としての意味を job 作成が引き継ぐ)。
>   upload 残骸が原理的に生じないため、[10-operations.md §7.5.2](10-operations.md) の
>   upload 照合・orphan 掃除は embedding 行に適用しない (`list_uploads` / `delete_upload` は
>   embedding の Batch trait に含めない)。相 1 / 相 2b / 相 3 と
>   [04-pipeline.md §5.8](04-pipeline.md) の状態機械はそのまま適用する。
> - 照会: `GET {base}/v1beta/{name=batches/*}`。state は
>   `BATCH_STATE_PENDING | RUNNING | SUCCEEDED | FAILED | CANCELLED | EXPIRED`。
> - 回収: `inlinedResponses[]` の `output.response.embedding.values` を `metadata` で入力へ対応づける。
>   受入検査は本節の (1)〜(5) をそのまま適用する (id 全単射・次元・有限非ゼロ・正規化後再検査・profile 一致)。
> - inline の上限 (20 MB) に収まるよう、1 job あたりの request 数を実装側で有界にする。
> - **task 単位 = job (2026-07-25 確定)**: §5.5 の v1 契約「1 job = 1 task」を embedding でも
>   保つため、**job そのものを 1 task として台帳に載せる**。`batch_requests.input_hash` は
>   当該 job のメンバ (= 各 group の `embedding_hash`) を整列・連結した digest とする。
>   embedding の自然な task 粒度は重複排除 group であり実規模で数千個 (dogfood fixture で
>   2,321) になるため、group 単位で job を作ると provider job が数千個になる。job 単位に
>   することで §5.8 の状態機械・crash 回復・`kio batch abandon` が無改造で適用できる。
>   同じメンバ集合を選び直した再実行は同じ task キーに落ち (= 宙に浮いた予約を再利用し
>   二重予約しない)、別の集合は別 task となる。先行 job が埋め込み済みのメンバは
>   content-addressed reuse で選択前に脱落するため無駄な再送は生じない。
> - **終端で `intent_token` を NULL 化する。** §5.8 の「NULL 化は残骸掃除の完了時のみ」は
>   upload 残骸を持つ OCR レーンの規則であり、inline の embedding レーンは残骸を作らない
>   ので**終端 = 掃除完了**である (§5.4 が sync 行に与える規則と同じ理由)。NULL 化しないと
>   「旧 token の消し込み完了後にのみ再投入可」と衝突し、同一 task キーの再投入が恒久停止する。
> - **レーンが使えない場合は sync へ落とす。** batch client を解決できない実行では、
>   `markdownize_send_lane` の `(batch client 無し, 開いた行無し) → Sync` と同じ規則で
>   同期レーンへフォールバックする (単価は sync の 2 倍で記帳する)。仕事を永久に pending の
>   まま残すより安全側。レーン可用性の判定は**認証不備の報告経路にしない** — 資格情報の
>   解決失敗は送信経路が R13-2(e) で報告する。
> - **課金報告**: `:asyncBatchEmbedContent` の応答もトークン数を返さないため `usage` は
>   引き続き `None` に degrade する。**単価はレーンに依存する** ため、見積り (= 確定記帳)
>   はレーンを見て $0.10 / $0.20 を選ぶこと。実装が単価を定数で持つのは誤りで、
>   `tools.toml` の `[embedding.*.pricing] tokens_in` を正本とする (§4 の billable_units 換算と同じ扱い)。
>
>   **宣言値の解釈 (2026-07-25 R24 で確定)**: `[pricing]` のキーは
>   `pages` / `tokens_in` / `tokens_out` の閉じた enum であり (`PRICING_KIND_ENUM`)、
>   **レーンの次元を持たない**。したがって 1 つの宣言で両レーンの単価を書き分けることは
>   できない。宣言された `tokens_in` は **標準 (sync) 単価**と解釈し、**Batch レーンは
>   その 0.5 倍**として導出する。宣言が無い場合のみ既定値 ($0.20 / 1M) に落ちる。
>
>   > R24 で 3 系統が fatal として指摘した実装欠陥: 宣言値をレーン判定より**前に**
>   > return していたため、`tools.toml` に `tokens_in` を書いた瞬間に両レーンが同額に
>   > 潰れていた。`usage: None` により見積り = 確定記帳なので、Batch ジョブが実額の
>   > 2 倍で記帳され、budget cap が守る金額が倍速で消費される状態だった。
>
> - **失敗ジョブの再投入は有界にする (2026-07-25 R24 で確定)**: 非成功終端は行を settle し
>   `intent_token` を NULL 化する。一方 §5.8 相 1 の `ON CONFLICT` は `batch_job_id` を
>   NULL に戻すため、**同じ task キーが即座に再予約・再投入できてしまう**。`usage: None` で
>   見積りがそのまま確定記帳になる以上、これは「成果ゼロの記帳が pass ごとに増え続ける」
>   ことを意味する。`batch_requests.attempts` は当該 `ON CONFLICT` の SET 対象外＝
>   **再予約をまたいで保存される**ので、これを上限 (既定 3) と突き合わせて打ち切る。
>
> - **回復走査の対象に含める (2026-07-25 G1 で確定)**: embedding の Batch client も
>   [10-operations.md §7.5.2](10-operations.md) の provider inventory に**必ず載せる**。
>   相 2b 開始済み・job 作成前で中断した行は `batch_poll_candidates` の対象外
>   (`batch_job_id` が無い) なので、**回復は `kio ledger reconcile` だけが担う**。
>   inventory に載っていないと `unlistable` のまま恒久的に宙吊りとなり、予約が
>   device budget cap を食い続ける。attribution は **job の `displayName` に埋めた
>   intent_token** で行う (Gemini の job は task key 4 組を運べる metadata を持たない)。
>   §5.8 の回復方向は intent_token 一致で成立するので、これで足りる。
>   逆方向 (provider 側 job の orphan 判定) で帰属できない job は upload と同じく
>   `unknown` として**報告のみ**とする。upload は inline のため常に空である。
>
> - **provider エラーは分類して保持する (2026-07-25 G2 で確定)**: 送信・照会の
>   provider エラーで**実行全体を落としてはならない**。§5.8 の unknown 規則
>   (「何も変更せず保持し、次回再試行する」) を OCR レーンと同じく適用する。
>   - 送信 (`provider_scope_id` / job 作成) の失敗は**その job だけ**を諦め、
>     同じ pass の他の job は続行する。分類は同期レーンと同一 (rate_limit は
>     `Pending + next_retry_at`、auth_error は `Paused(hold_reason=auth)`、他は Failed)。
>     予約は保持し、回収は上記の回復走査に委ねる (F8 の reserve-before-send 姿勢)。
>   - 照会 (`get_job` / 結果取得) の失敗は**その行だけ**を in-flight のまま保持し、
>     次の行の処理を続ける。1 行の不達が同一 scope の残り全部の回収を止めてはならない。
>   - ネットワークエラーを `KIO-E-CONFIG-SCHEMA-001` のような恒久エラーとして
>     報告しない。
>
> - **回収は入力との全単射を検査する (2026-07-25 R24 で確定)**: 本節 (1) の全単射契約は
>   collect 側にも適用する。task キーが**メンバ集合の digest** である (上記「task 単位 = job」)
>   ことを利用し、**回収できた結果から digest を再計算して行の `input_hash` と突き合わせる**。
>   canonical な全 id が一意で、完全な集合の digest が一致することを、最初の CAS / DB 書込みより
>   前に検証する。現在の profile に属する応答は全 vector の受入検査・正規化も先に完了する。
>   不一致・欠落・余分・重複・不正 vector は、応答全体を `Outcome::ContractViolation` /
>   `BatchState::Terminal` として保守的な予約見積りで精算する。部分保存しない。
> - **契約違反後の新規 Batch 停止**: 同じ scope / embedding adapter に、未 reset の
>   typed contract violation が一件でもあれば、新しい Batch 予約・送信を停止する。
>   判定は terminal 行の `contract_violation_count > 0` と、その完全な task key / 現在の
>   `submission_seq` に一致する cost ledger の outcome に基づく。自由文の error は判定に使わない。
>   新しいメンバ集合・profile へ組み直しても解除しない。digest だけから不正応答の元メンバを
>   推測しないための scope 単位の停止規則であり、既に送信した job の照会・精算は継続できる。
>   明示した sync レーンにはこの Batch 停止を適用せず、その承認・予算・再試行規則を適用する。
>   `KIO-E-EMBED-BATCH-CONTRACT-HOLD-001`（exit 4）は対象 attempt と完全な selector を示す。
>   原因確認後の `kio batch retry --reset-violations <selector> --yes` で該当 count を解除する。
>   過去の cost ledger は削除しない。通常の provider 失敗・期限切れはこの停止条件に含めない。
>
> - **実物の wire 形 (2026-07-25 実 API 実行で確定)**: 上の「照会」「回収」の記述は
>   公式リファレンスの語彙に沿ったもので、**実際の応答とは 3 点ずれていた**。実行時に
>   採取した応答を正本とし、次を契約とする (`gemini_batch_client.rs` の `real_*` テストが
>   採取した応答そのものを固定している)。
>
>   1. **すべて long-running-operation の封筒である。** job レコードは最上位ではなく
>      `metadata` の下にある (`metadata.state` / `metadata.displayName` / `metadata.name`)。
>      完了後は `response` にも出力の複製が現れるが、こちらは `state` も `name` も持たない。
>   2. **一覧は `operations` キーで返る** (`batches` ではない)。`batches` だけを読むと
>      **常に空ページ**になり、回復走査は「回収すべき job は無い」と解釈して**黙って**
>      何もしない (§7.5.2 の inventory が恒久に空になる)。
>   3. **結果配列はキー名より 1 段深い。** `inlinedResponses` は出力先オブジェクトの名で、
>      その中の `inlinedResponses` が配列である
>      (`metadata.output.inlinedResponses.inlinedResponses[]` および
>      `response.inlinedResponses.inlinedResponses[]`)。各要素は
>      `{ response: { embedding: { values }, usageMetadata }, metadata: { key } }` で、
>      `output` でのラップは**無い**。
>
>   > 実装欠陥として観測された帰結: 2 と 3 により、**実 job は 1 件も回収できない**。
>   > 3 は `fetch_inlined_results` を contract violation にし、照会側の G2 規則
>   > (「その行だけ in-flight のまま保持」) がそれを飲み込むため、**終端した job が
>   > 診断なしで永久に in-flight のまま滞留する**。契約テストは全て通っていた —
>   > モック seam が `{"inlinedResponses": [...]}` という**実 API が返さない最内側の形**を
>   > パーサへ直接与えていたためである。**モックは provider と同じ封筒を通すこと**
>   > (seam が wire から乖離した瞬間、その経路のテストは何も保証しなくなる)。
>
> - **応答はトークン数を返す (2026-07-25 実 API 実行で訂正・同日実装)**: 下の「課金報告」は
>   `usage` を `None` に degrade すると述べているが、**両レーンとも事実に反する**。
>   Batch は各結果行に `response.usageMetadata.promptTokenCount` を持ち、同期の
>   `:batchEmbedContents` は**呼び出し単位の合計**として同じフィールドを返す。
>   同期側の旧コメント「per-request のトークン数が無い」は字義どおり正しいが、
>   **課金の粒度は呼び出し単位**なので「自己申告する信号が無い」という結論が誤りだった。
>
>   したがって embedding は**両レーンとも実測トークンで確定記帳する**。ただし次の 3 つは
>   保守側 (予約見積り) を維持する — いずれも**過少記帳**になる経路だからである:
>   (a) 全単射が不成立の回収 (欠けた結果のトークン合計は本来の請求を下回る)、
>   (b) usage 報告が無い応答 (0 と解釈すると $0 で確定し、cap が既発生の支出を解放する)、
>   (c) 一部の行にしか報告が無い場合 (部分合計は静かに過少になる — all-or-nothing で退避)。
>
>   **予約見積りは保守側ではない (2026-07-25 実測)**: 33 実ジョブで突き合わせた
>   `estimate_embedding_tokens` の誤差は **-32% 〜 +52%**、**8/33 (24%) が過少**だった。
>   確定記帳は実測へ移ったが、**budget cap の事前判定は依然この見積りを使う**ため、
>   過少なジョブは cap を素通りしうる。
>
> - **単価はレーンで割る — 宣言値は標準 (sync) 単価 (2026-07-25 統一)**: 確定記帳の経路には
>   レーン概念が無く、両 settle site が `registered_declared_pricing` を**生で**読んでいた。
>   markdownize でそれが正しく見えていたのは宣言値がたまたま Batch 単価だったためで、
>   その結果 **`--realtime` の OCR は実額の半分で記帳されていた**。過少記帳は budget cap が
>   構造的に防げない方向 (cap は小さい方の数字しか見ない) なので、`lane_adjusted_pricing`
>   を単一の適用点とし、**全 role で「宣言値 = 標準単価、Batch = 0.5 倍」に統一**する。
>   `tools.toml` の `[markdown.*.pricing] pages` は $4 / 1,000 pages (= 0.004) を宣言すること。
>   予約と確定は**同じ単価解決を通す** — 別々の源から引くと両者が乖離する。
>
> **レーンは invocation 単位で 1 つ (2026-07-24 ユーザー裁定)**: OCR と embedding を
> **別レーンに分けない** — 1 回の実行では**両方 Batch か、両方即時か**のいずれかである。
> したがってレーン選択は adapter ごとの `preferred_request_kind()` を既定値としつつ、
> **実行時に CLI 引数が両方まとめて上書きする**。
>
> - 既定 = **Batch** (OCR $2/1,000 pages・embedding $0.10/1M tokens。turnaround は最大 24h)。
> - `kio index --realtime` / `kio batch resume --realtime` / `kio batch retry --realtime` =
>   **両方を即時レーンへ倒す** (OCR $4/1,000 pages・embedding $0.20/1M tokens = いずれも 2 倍)。
> - **これは 2026-07-23 の「OCR 課金は Batch レーンのみ・sync を本番送信に使わない」裁定を
>   明示的に上書きする。** 即時性が要る実行では倍額を承知で sync レーンを使う、が新しい規範。
>   ただし**既定では従来どおり Batch のみ**であり、倍額は明示 opt-in でしか発生しない。
> - `--realtime` は `--online` とは**別の軸**である: `--online` は「送ってよいか」
>   (network opt-in、§3)、`--realtime` は「いつ返ってくるか」(turnaround)。
>   API 経由である以上 online は前提なので、即時性の指定に `--online` を流用しない。
> - **飛行中の batch 行は `--realtime` で乗り換えない** — 既に provider job を持つ
>   `request_kind='batch'` 行を sync へ移すと、その job が [04-pipeline.md §5.8](04-pipeline.md)
>   の回復から外れて残骸化する。`--realtime` が効くのは**新規送信のレーン選択のみ**。
> - 例外 = **query embedding は常に即時**。検索は batch の turnaround を待てないため、
>   `kio search` の query 埋め込みはレーン裁定の対象外で、常に sync 単価で記帳する。

embedding の SQLite schema (`embeddings` / `chunk_vec`) の正本は [04-pipeline.md §4.3](04-pipeline.md)
とする (本節は profile — モデル / 次元 / 距離 / modality — の正本。SQL 定義の重複記載は 2026-07-14 に
解消し、本節から参照する)。

sqlite-vec の制約で vector table を物理分割してもよいが、概念上は単一の Embedding Adapter / 単一の `profile_hash`。profile が一致しない場合、Kio は vector 検索を強行せず再生成または text fallback。

> **実装フィードバック — chunk 埋め込みの contextual 化 (2026-07-24)**: MVP の実データ
> ベースライン評価 (09 §4.1) で、vector レーンが「本文 1〜2 文＋定型 filler」の実機規模
> (人格あたり 50 ファイル) で薄い解答本文を浮上させられず、`Kio ≥ 0.8` 腕が未達だった。
> **ファイル名はユーザーが付けた意図信号**であるため、chunk の埋め込み**入力**を
> `{humanized filename}\n\n{chunk 本文}` とする (contextual retrieval)。`humanized filename`
> は basename stem の決定的変換 (`-`/`_`→空白、ASCII camelCase 境界→空白、空白畳み込み。
> 英数字を含まない純記号名は変換不能として prefix なし)。凍結 24 問での事前計測: vector 単独
> file-level Recall@10 が **14/24 → 23/24**。query 側は従来どおり素の埋め込み (ファイル名を
> 持たない)。
>
> - **同一性**: chunk ベクトルは `(text_hash, context, profile)` の関数になるため、
>   content-addressed identity (`embedding_hash`) に `context` を畳み込む (`None` = 従来と
>   バイト同一のハッシュ)。異なるファイル名の同一本文が同じ `id` に衝突して 1 ベクトルを
>   共有する事故を防ぐ。
> - **profile**: 本節の profile が `input_construction = "chunk_filename_context_v1"` を宣言する
>   (04 §4.1 の識別フィールド許可リストに追加、embedding 専用で他 profile のハッシュ不変)。
>   これにより (a) 03 §7 の互換ゲートが非 contextual な旧ベクトル空間を **incompatible** と
>   正しく判定し、(b) profile hash 変化が全 chunk の再埋め込みを再起動する。
> - **本文の構築**: 認証済み chunk CAS の本文から、NFC 正規化 → NUL 除去 → Markdown の
>   escape / entity 解決の順で検索用本文を導出し、上記 filename prefix と結合する。
>   fenced block / inline code 内の escape は解決しない。FTS と embedding は同じ純粋関数を使い、
>   SQLite の `chunks.text` を送信本文の正本として読まない。`text_hash` は変換前の認証済み
>   本文に対する hash のままとする。この構築規則を変更するときは `input_construction` を更新し、
>   同じ profile / embedding identity に異なる入力を混在させない。
> - **content dedup の縮退**: cross-file の content-twin 再利用 (CT3-EMBED-006 / R19-4) は
>   同一 `(text_hash, context)` = 実質同一ファイル名に縮退する。これは意図した挙動 (contextual
>   ベクトルはファイルごとに異なるべき)。twin 収束の自己修復経路自体はコード上は健在
>   (同一 context の twin で発火)。
> - **rebuild/snapshot**: 一つの `text_hash` が複数の filename context を持ち得る。
>   保存済み embedding を全 context について再構築し、現行 policy が許可する HEAD の名前を優先して
>   scalar 検索 vector を決定する。詳細は [04 §4.3](04-pipeline.md) の選択規則に従う。
>   候補が一つでも context 不一致を受理せず、応答順序による上書きで選択しない。
>
> **ローカル multimodal embedding の identity 規約 (2026-07-26 確定)**:
> `execution_mode = "offline_api"` の Embedding Adapter (ローカル embedding server) を
> 導入するための規約。**3 項目とも「同一 profile_hash を名乗る 2 つのベクトル空間」を
> 生まないための規範**である — [03-data-model.md §7](03-data-model.md) の互換ゲートが
> 見るのは次元 / distance / modality / `profile_hash` の 4 つだけなので、
> 同じ profile を名乗る限り**空間の分裂を原理的に検知できない**。しかも embedding は
> content-addressed identity を持ち first-instance-wins で永続化されるため、
> 誤った空間のベクトルは**恒久的に凍結される**。事後検知に頼らず構成段階で塞ぐ。
>
> **(1) chat template と instruction を `prompt_template_hash` へ畳み込む。**
> ローカルの multimodal embedding は chat template でラップされて初めてトークン列が
> 決まり、タスク別 instruction (例: `"Represent the user's input."`) の有無・文面が
> 出力ベクトルを変える。**同じ重みでも template が違えば別のベクトル空間である。**
> [03-data-model.md §5.1](03-data-model.md) の `prompt_template_id` /
> `prompt_template_hash` に固定する (どちらも既存の hash 入力フィールドであり、
> 現行の cloud embedding profile では未設定のまま — ローカル profile で初めて埋まる)。
>
> **結合規則を固定する** — 区切り文字連結は実装差を生むため、次の 2 key object を
> JCS した byte 列の sha256 を `prompt_template_hash` とする
> (§5.2 の `bbox_annotation` が `sha256(JCS(json_schema))` を採るのと同型 —
> このフィールドは artifact 種別ごとに異なる canonicalization を既に許容している)。
>
> ```json
> { "chat_template": "<template 本文>", "instruction": "<instruction 本文>" }
> ```
>
> 各値は [03-data-model.md §5.1](03-data-model.md) の `prompt_template_hash` 前処理
> (行末空白除去 → 改行正規化 → NFC → 末尾空行削除) を**適用してから** object に入れる。
> instruction を使わない構成では `"instruction": ""` を明示する (key の省略と空文字を
> 識別しない — 同節の null 規約と同じ姿勢)。
>
> > **実測による裁定 (2026-07-27・V4)** — Qwen3-VL-Embedding-2B は
> > **`instruction: ""` を採る**。`prompt_template_hash` は
> > `sha256:7b7f4722…9e8b`、`tool_profile_hash` は `sha256:f9f610bb…439a`
> > (`dimensions` は V3 決着まで暫定 — [eval/v4/results/](../eval/v4/results/README.md))。
> >
> > 本節が instruction の例に挙げた `"Represent the user's input."` は、実測の結果
> > **モデルの chat template が system message 不在時に注入する
> > `default_system_message`** だと判明した。つまりこの文字列は **T の一部であり、
> > 既に hash 入力に入っている**。Kio は system message を送らない (テキスト・画像とも
> > `messages: [{"role": "user", ...}]` の 1 通) ので、Kio が供給する instruction は
> > 無く、上段が「`""` を明示する」と規定している当のケースに該当する。
> >
> > 非空を採っても**ベクトルは完全に同一**である (template が同じ文字列を注入するため)。
> > 違うのは identity だけで、非空は 1 つの事実を T と instruction の 2 か所に記録し、
> > かつ後から読む者に「Kio がこの instruction を選んだ」と誤読させる。実際には
> > モデル側の既定を観測しただけである。
> >
> > **本項 (1) の規範自体は反証されていない。** 同じ probe で instruction の文面だけを
> > 非既定に変えると cos は 0.7989 まで落ちる — instruction は実際にベクトルを動かす。
> >
> > **したがって「system message を送らない」は実装への拘束である。** 送るようにするなら
> > その文面が `instruction` であり、`prompt_template_hash` を計算し直さねばならない。
> > 送りながら `""` を記録すると、profile が実際のトークン列を偽ることになる —
> > 本節冒頭が塞ごうとしている失敗そのもの。
>
> **(2) wire 形式を `messages` に一本化する。**
> ローカル embedding server の OpenAI 互換エンドポイントは、テキストを
> `input: ["text"]` で受ける一方、**マルチモーダル入力には chat 形式の `messages` を
> 要求する**。`messages` は chat template でラップされるため、**同じモデル・同じ文字列でも
> `input` 経由と `messages` 経由ではトークン列が異なりベクトルが異なる。**
>
> テキストチャンクを `input` で埋め込み、後から画像を `messages` で追加すると、
> 同一 profile を名乗ったまま実質 2 空間に分裂する。したがって
> **テキストのみのチャンクでも常に `messages` 形式を使う。** 代償は 1 リクエスト
> 1 アイテム (バッチ不可) だが、Kio の送信は既に重複排除 group ごとに 1 コールであり、
> ローカルサーバ側の continuous batching が吸収する。
>
> > **実測による確認 (2026-07-27・V4)** — Qwen3-VL-Embedding-2B / vLLM 0.26.0 で
> > `cos(input[] 経由, messages 経由)` = **0.4740**。同じ probe の
> > `cos(無関係な 2 文, ともに messages 経由)` = **0.5966** なので、
> > **同一文字列を 2 つの wire 形式で通すほうが、文章の内容を丸ごと差し替えるより
> > 遠い**。本項が払うバッチ不可のコストは実際に空間の分裂を防いでいる。
> > (両者は `messages` 経由の同一ベクトルを共通項に持つので比較は成立する。)
> >
> > エンドポイント形状も確定した — vLLM 0.26.0 では `/v1/embeddings` が
> > `messages` をそのまま受け、`/pooling` への fallback は要らなかった。
> > 測定の全文は [eval/v4/results/](../eval/v4/results/README.md)。
>
> **(3) serving backend は identity に含めない。**
> [03-data-model.md §5.1](03-data-model.md) が「実装バイナリのバージョン・OS・ハードウェアは
> `binary_hash` として別保存し `tool_profile_hash` には含めない」と規定している系として、
> **同一の重み・同一の chat template であれば、どの推論ランタイムが生成したベクトルでも
> 同一 profile として扱う。** これにより serving backend の乗り換え (別 OS への移行等) が
> **再埋め込みなしに**成立する。
>
> したがって **`model_or_tool_family` をはじめ、いかなる hash 入力フィールドにも
> backend 名を混ぜてはならない** (`"qwen3-vl-embedding-vllm"` のような命名の禁止)。
> 混ぜると backend を替えただけで profile_hash が変わり、互換ゲートが全 chunk の
> 再埋め込みを要求する。
>
> **ただし identity が同じことは数値が同じことを保証しない。** backend を乗り換える際は
> 参照実装との数値一致を確認する責務が実装側にある — 本節が定めるのは identity の
> **規則**のみであり、検証手続きは別途とする。「テキストは一致・画像だけ乖離」という
> 失敗モードは本節冒頭のとおり互換ゲートで検知できないため、これは形式的な注意ではない。

## 5.4 Rerank (optional)

```
input:   query, candidate_result_ids, candidate_features
output:  reranked_result_ids, scores
metadata: profile_hash, searched_scopes, fallback_reason
```

Rerank Adapter は Kio の検索結果を再順位付けするだけで、**searched_scopes / fallback_reason を隠蔽してはならない**。

## 5.5 Batch 実行契約とプロバイダ採用条件

Batch モードを持つ online Adapter (Markdownize / Embedding) は、[04-pipeline.md §5.8](04-pipeline.md) の
2 相プロトコルが要求する次の操作を trait として公開する:

```
upload(bytes, filename)        client 指定の filename を受理する (intent_token 埋込のため)
create_job(inputs, metadata)   client 任意の metadata (intent_token + タスクキー 4 組) を job に付与できる
get_job(job_id) / list_jobs()  list は account/workspace scope 内の全件を pagination 走査でき、
                               どちらも create_job で付与した metadata を完全・不変に返却する
                               (帰属・orphan 報告の前提 — [04-pipeline.md §5.8](04-pipeline.md))
list_uploads()                 scope 内の upload を pagination 走査でき、filename (intent_token 埋込) で
                               照合できる — upload_id 記録前クラッシュの残骸発見の唯一の経路
delete_upload(upload_id)       404 (不存在) は削除成功として報告する
fetch_output(job_id)           出力 JSONL は入力ごとの custom_id (= unit_key) を保存して返却する
                               ([04-pipeline.md §5.8](04-pipeline.md) の unit 復元の前提)
provider_scope_id()            下記の versioned credential-bound recovery scope を返す
```

- **エラー分類の契約**: Adapter は失敗を transient (5xx / ネットワーク断)・rate_limit (429 —
  `Retry-After` があれば `retry_after_ms` で透過する)・permanent (内容起因の 4xx) の 3 分類で
  報告する (§4 の error_category と同一 enum)。分類と retry 予算の対応は
  [04-pipeline.md §5.3](04-pipeline.md) (rate_limit は Retry-After を解除条件とする retryable)
- **課金報告**: **request (job / sync 呼出) 単位** — 各 request の応答 / fetch_output に実測コスト
  (または単価計算に足る unit 数) を含めて報告する ([04-pipeline.md §5.4](04-pipeline.md) / §5.8 の
  request 単位記帳の前提 — 直列多 request task では各 request の終端 Tx が自身の実測 usage を持つ)。
  **機械契約は §4 AdapterRun の `usage` field (one-of: `usd` | `billable_units`) — billable な
  terminal 応答で必須**。報告値が cost ledger の記録源である
- **provider_scope_id**: account の恒久 ID とは別の、versioned credential-bound recovery scope。
  `kio-batch-recovery:v1:<provider>:<HMAC-SHA256>` とし、device-private な専用 32-byte
  `batch-recovery-key` を鍵に、domain / provider namespace / canonical HTTPS origin /
  optional workspace・project qualifier（有無を区別）/ exact credential bytes を length-framed で結合する。
  client 構築時に private source の検証後、credential / origin / qualifier を一度だけ capture し、
  upload / create / poll / fetch / delete の全呼出で同じ値を使う。環境変数名や表示名の一致だけでは同一 scope としない。
  HMAC は central ledger の回復照合専用であり、`.kio`、tool profile、send grant、通常 JSON / Debug 出力には含めない。

**Batch プロバイダ採用条件** (満たさない provider は sync 呼出のみで採用するか、採用しない。**例外 = 条件 7 は sync fallback の免除対象外**: sync 呼出も provider request id を同じ記帳キー (`batch_job_id` 列) と cost_ledger 恒久突合に使うため ([04-pipeline.md §5.4](04-pipeline.md))、条件 7 を満たさない provider は sync でも採用しない):

1. job 一覧照会 (または token による job 発見) と **upload 一覧照会**が可能であること — これが無いと
   未記録 in-flight・未記録 upload 残骸の回復が構造的に不可能になる
2. job 作成 → 一覧可視化の遅延に上限があること (Kio の可視化猶予 既定 10 分以内)。**upload →
   一覧可視化にも同じ上限を適用する** ([04-pipeline.md §5.8](04-pipeline.md) の upload 照合が前提とする)。
   upload() は upload_id を返し (返却型必須)、list_uploads は pagination を提供すること
3. job / 一覧情報の保持期間が Kio の回復期限 (既定 48h) 以上であること
4. job metadata / filename に client 任意の識別子 (intent_token) を埋め込めること
5. 上記の versioned credential-bound recovery scope を提供し、記録済み scope と現在 client の完全一致を
   poll / fetch / delete 前に検証できること。credential / origin / qualifier / device key の変更は別 scope であり、
   自動 cross-key rotation・rebind はしない。旧 constant scope との互換 fallback も設けない。
   credential を rotate する前に pending job / upload cleanup を drain する。未解決行がある場合は、
   元の authorized credential と device key を復元するか明示 `kio batch abandon` で扱い、別鍵へ自動再送しない。
6. 投入拒否 (permanent 4xx) にも課金するか否かを宣言すること。**課金する provider の Adapter は、
   拒否応答時に usage (`usd` = 宣言請求額 | `billable_units` — §4 の one-of と同形、第三の field は
   設けない) を機械可読で返却する** (この返却義務は Batch 限定で
   なく **sync online Adapter にも共通** — [04-pipeline.md §5.4](04-pipeline.md) の sync 記帳規律が参照する) — Kio は submit_rejected の
   terminal 化と同一 Tx で記帳する (**報告値が有効なら provider 値 (`estimated=0`)、無効・欠落は
   estimated 縮退** — [04-pipeline.md §5.4](04-pipeline.md) の事前検証と DDL 注記)

7. job id / provider request id が、同一 adapter_kind 内で account / workspace を跨いで**恒久に
   実質再利用されない**こと ([04-pipeline.md §5.8](04-pipeline.md) の記帳済み判別が task key × job id
   を**恒久保持の cost_ledger 全履歴**に対して突合するため — 期限付きの一意性では期限超の再利用が
   過去行と誤合致して確定記帳を落とす)

`mistral_ocr_markdownize` の Batch モードは 2026-07-03 の実地検証 (§5.2 末尾) の範囲でこの条件下で
採用済み。

> **Batch レーンの実装確定 (実装フィードバック 2026-07-23)**: `mistral_ocr_markdownize` の
> 本番送信は Batch レーンに固定した (同日ユーザー裁定「OCR の課金は Batch 実行のみ許可」 —
> $2/1,000 pages。**bbox 有効タスクも例外にしない** — bbox annotation は既定 ON のため、例外に
> すると本番が sync に留まり裁定が空文化する。batch 入力 body は sync と同じ
> `bbox_annotation_format` を同梱)。レーン選択 = 「Batch client が構成済み (markdown role の
> 認証解決可) なら Batch」。`request_kind='sync'` の生存予約行のみ §5.8 の回復意味論保全のため
> sync で完遂する。v1 確定事項: **1 job = 1 task** (`batch_job_id` 列がそのまま回復キー)、
> custom_id = task の input_hash、job metadata = flat 5 key (`intent_token` / `scope_id` /
> `adapter_kind` / `input_hash` / `tool_profile_hash`)、upload filename = `kio-<intent_token>.jsonl`。
> 単価はコード内に per-page 定数の正本が無いため、tools.toml の `[pricing] pages = 0.002`
> (batch 単価) 宣言を推奨する。終端の記帳: provider `TIMEOUT_EXCEEDED` は出力を読めず実費不明 =
> `expired` として**予約額の estimated 記帳** (§5.8 と同じ over-count 安全側)、`FAILED`/`CANCELLED`
> は未完了 entry 非課金の前提で 0 記帳。

---

# 6. tool-lock.json

`.kio/tool-lock.json` は使用 Adapter の identity を記録する。実行可能情報 (`cmd`, `args`, `url`, 認証情報) は **絶対に含めない**:

```json
{
  "spec_version": 1,
  "prepare": {
    "tool_id": "prepare_default",
    "kind": "deterministic_library",
    "profile_hash": "sha256:..."
  },
  "markdown": {
    "tool_id": "mistral_ocr_markdownize",
    "kind": "online_api",
    "profile_hash": "sha256:...",
    "capabilities": ["ocr", "layout_detection", "table_extraction"]
  },
  "embedding": {
    "tool_id": "gemini_embedding_2",
    "kind": "online_api",
    "mode": "online",
    "dimensions": 768,
    "distance": "cosine",
    "modality": "multimodal",
    "profile_hash": "sha256:..."
  }
}
```

`tool_lock_hash` は **[03-data-model.md §5.2](03-data-model.md) の canonical 入力** (spec_version + 各 role の tool_id / profile_hash — embedding のみ + dimensions / distance / modality) を JCS 畳み込みした identity — **tool-lock.json 全体ではない**。`kind` / `capabilities` / `mode` は作業コピーの表示・検証用 field であり identity に含めない (toollock object へ保存されるのも §5.2 の canonical bytes)。

config (`~/.config/kio/tools.toml`) では `mistral-ocr-latest` のような可変 alias を指定してよい。ただし **OCR API は応答内で alias を実バージョンに解決しない** (2026-07-03 実測: 応答の `model` フィールドは `mistral-ocr-latest` のまま返る。`experiments/ocr-verification`)。したがって Adapter は **API 呼び出し自体を版付きモデル名で行う**: alias が設定されている場合は、Adapter が実行開始時に提供元のモデル一覧 API から現行の版付き名を解決してから呼び出し、その版を `tool_profile_hash` の `model_version_pin` に記録する ([03-data-model.md §5.1](03-data-model.md) — 可変 alias の pin は禁止)。モデル更新は `tool_changed` として扱われ、再 Markdownize は first-instance-wins / gen の既存機構 (§9) に乗る。

---

# 7. Adapter 実行制約 (policy)

```toml
[adapter.policy]
allow_network = false
allowed_scope = "."
max_input_bytes = 104857600        # 100 MiB (= 100 * 1024 * 1024)
timeout_seconds = 300
redact_logs = true
store_request_body = false
store_response_body = false
require_command_confirmation = true
```

> **execution_mode 別の上書き (2026-07-26 確定)**: `[adapter.policy]` は全 Adapter 共通の
> 既定だが、CPU 推論のローカル VLM は 1 ページの OCR で `timeout_seconds = 300` を超え得る。
> **`[adapter.policy.<execution_mode>]` の sub-table による上書きを認める** (`online_api` /
> `offline_api` / `deterministic_library`)。未指定のキーは親の値を継承する。
>
> ```toml
> [adapter.policy]
> timeout_seconds = 300
>
> [adapter.policy.offline_api]
> timeout_seconds = 1800          # 親を上書き。他のキーは親から継承
> ```
>
> **enforcement の単位は変えない** — §7.1 のとおり AdapterRun 1 回 (= 1 request / job) に
> 適用する。上書きが変えるのは値だけであり、適用対象・適用時点は共通である。
> `offline_api` の既定値は Stage 3 のローカル OCR 実測をもって確定する
> ([tasks/local-adapter-plan.md](../tasks/local-adapter-plan.md))。
> なお `max_input_bytes` など他のキーも同じ機構で上書きできるが、**現時点で
> execution_mode 差を必要とするのは `timeout_seconds` のみ**である。
>
> **実装状況 (2026-08-02)**: 配線されているのは **`offline_api` の
> `timeout_seconds` だけ**である。`online_api` / `deterministic_library` の
> sub-table は `KIO-E-CONFIG-NOT-IMPLEMENTED-001` で**拒否する** — 何も honour
> しない値を受理すると、現在の大声の拒否が黙った no-op に退化し、運用者は効いて
> いない上限が効いていると信じることになる。親の `timeout_seconds` も従来どおり
> 既定の 300 以外を拒否する (広げると課金の走る online adapter の挙動まで変わる
> ため、D7 の範囲外)。sub-table が無い場合は親を継承 = 既定のままで、**D7 以前と
> 挙動は 1 ミリも変わらない**。`offline_api` の既定値は上記のとおり Stage 3 待ちで、
> こちらで発明していない。

R23 の Markdownize / Embedding runtime は同梱 target だけを実行し、任意コマンドの実行経路を持たない。

ログに残してよいもの:

```
task_id, adapter_id, tool_profile_hash, execution_mode, scope_id
input_raw_hash, output_hash
status, error_code, error_category, retry_after_ms
network_consent (current paired grant identity — HTTP dispatch を伴った実行のみ)
started_at, finished_at
adapter_kind, input_hash, intent_token, submission_seq
usage_validation (missing | invalid), billing_source (estimated)
                             (非機微 — 課金 field 縮退の warning ([04-pipeline.md §5.4](04-pipeline.md)
                              の記帳値事前検証)。event code は KIO-EV-ADAPTER-USAGE-001)
                             (非機微。**cost_ledger / batch_requests への到達は 4 組 key
                              (scope_id, adapter_kind, input_hash, tool_profile_hash) +
                              submission_seq が正** — intent_token は補助 (成功行の batch_job_id は
                              provider request id を優先し token を保持しない)。input_raw_hash は
                              raw 由来 task 用で、device 行 (query embedding) の input identity は
                              input_hash 側。CAS で敗れた送信 ([04-pipeline.md §5.4](04-pipeline.md))
                              は回収側の unknown_settled 行に対応づく — 送信 log 件数と cost_ledger
                              行数が一致しないのは二重課金防止のための意図的挙動)
```

`adapter_id` は tools.toml の `tool_id` と同一値である (別 namespace を作らない — scope approval reference と device grant (§3) の照合キーと一致し、実行 Adapter を paired authority へ一意に対応付ける)。

残してはならないもの:

```
原文本文 / normalized 本文 / API request body / API response body / 秘密情報
```

## 7.1 強制モデルと信頼境界 (MVP)

MVP における Adapter の脅威モデルを次のとおり確定する。

```text
1. R23 で実行される Adapter は Kio 同梱の built-in target のみを trusted code として扱う。

2. [adapter.policy] は「Kio 側の入力制御 + 事後監査」の規約であり、
   sandbox による強制保証ではない。
   - `max_input_bytes` は **AdapterRun 1 回の入力 (prepared input の canonical bytes 合計)** に
     適用する (**AdapterRun = 1 回の Adapter 呼出 = 1 request / job** — §4・§5.5 の課金報告と同一
     単位。task 全体の総量上限ではない — 総量は budget cap 側が律する) — 超過は送信前に当該 task を
     terminal failed (invalid_input・非再試行) とし、送信しない (課金なし)
   - Kio は allowed_scope 外のファイルを Adapter に渡さない (入力制御)
   - Kio は永続 gate が不成立の Adapter にオンライン送信前提の task を発行しない
   - AdapterRun (task_id / input_hashes / output_hashes / status) を監査ログとして残す

3. Kio が実行・文書化するのは同梱 built-in Adapter だけである。
```


---

# 8. Incremental Markdownize プロンプト規約

[04-pipeline.md §3.1](04-pipeline.md) で発動条件と入出力 schema を定義した。本節は **Adapter 内部のプロンプト規約** を固定する (Adapter ごとの揺れを防ぐため)。

**適用範囲**: 本節のプロンプト規約は**生成 LLM 系 Markdownize Adapter** に適用する。文書処理 API 系 (Mistral OCR 等、§5.2) は unit (page) fingerprint の再利用により変更 unit のみを再処理する経路 ([04-pipeline.md §2.2](04-pipeline.md)) で incremental を実現するため、プロンプト規約は適用されない。ただし §8.1 の 6 (受け入れ検査) と入出力 schema は**全 Markdownize Adapter 共通**。

## 8.1 Adapter が守るべき規約

```
1. "unchanged" と判断した unit は出力に含めない (旧 unit を再利用)
2. 変更 unit は完全に書き直す (部分編集ではなく full unit replacement)
   → Markdown の局所一貫性を保つ
3. heading 構造の変更は Kio には影響しない (chunk side で対応)
4. Adapter が「軽微とは言えない」と判断したら fallback_to_full=true で短絡
   (受理側は unit 検査に先立つ制御応答として識別し、本文として保存しない。
   同期有料要求の後でFullを再送するには、一度の明示承認と新しい予約が必要 —
   [04-pipeline.md §3.2](04-pipeline.md) の制御応答規則。full 応答での本 flag は contract violation)
   閾値の Adapter 側 hint は Kio 側 hint と衝突したら **Kio 側を優先**
5. spec_version 不一致なら、Adapter は invalid_input として失敗 (`KIO-E-ADAPTER-SPECVER-001` — 汎用 `KIO-E-ADAPTER-CONTRACT-001` (retryable 1 回) と区別し、[04-pipeline.md §5.3](04-pipeline.md) の invalid_input 分類 = max_attempts 0 に一意に対応させる)
6. 出力は Kio 側の受け入れ検査 (04-pipeline.md §3.2) を通過しなければ persist されない。
   違反は KIO-E-ADAPTER-CONTRACT-001 として reject され、**incremental capability 非互換の場合に
   限り** full に fallback する (spec_version 非互換は下記のとおり fallback しない)
```

`spec_version` の bump 規約は [10-operations.md §11.5](10-operations.md) を正とする。**full fallback (§8.4) が有効なのは incremental capability だけが非互換な場合に限る** — full request も同じ `spec_version` を含むため、spec_version 自体の非互換は full で呼び直しても同じ invalid_input を再生するだけである。この場合は `KIO-E-ADAPTER-SPECVER-001` (§8.1 手順 5 — invalid_input / 非再試行) として当該 online Adapter のタスクを failed permanent (Adapter 更新が必要) にし、同梱 deterministic Adapter のベースライン (§2.1) は影響を受けず継続する。

## 8.2 推奨プロンプト構造 (frontier AI 系)

```
SYSTEM:
  You are a markdownization adapter for Kio.
  Given the previous markdown of <unit_key> and the new raw input,
  produce updated markdown for changed units only.
  Keep unchanged units out of the output.

USER:
  Mode: incremental
  Tool profile: <hash>
  Previous markdown for changed units:
    <unit_key_1>: <markdown_1>
    <unit_key_2>: <markdown_2>
  New raw content (relevant pages only):
    <raw_excerpt>
  Hints:
    changed_unit_keys: [...]
    page_fingerprints: {...}

  If you judge the change as non-minor, return fallback_to_full=true
  with a brief reason. Otherwise return updated_units.
```

具体的な system prompt は Adapter 実装で固定し、`prompt_template_hash` (`tool_profile_hash` 入力フィールド) で identity に含める。

## 8.3 ストリーミング応答

大型 PDF (100+ pages) では TTFB を抑えるためストリーミング出力を許容する。Kio は Adapter からの SSE / chunked JSON を受け取り、unit 完了ごとに persist する。

ストリーミング中の unit は staging 領域に persist し (**配置と耐久 descriptor は
[03-data-model.md §2](03-data-model.md) の `.kio/staging/` — purge / status / prune-orphans の
帰属列挙の正本**)、応答完了後に**全体集合が受け入れ検査
([04-pipeline.md §3.2](04-pipeline.md)) を通過した時点で manifest へ一括確定する** (検査前の unit は
公開しない — §3.2 の「違反応答は 1 unit も persist しない」と整合)。ストリーミング失敗時は staging を
破棄せず、**完了済み unit は保全したまま task を failed (retryable) にする。task が done (受け入れ検査
通過 → manifest 一括確定)・failed permanent・abandon・settled partial (全 unit が terminal —
[04-pipeline.md §5.2](04-pipeline.md)) のいずれかで terminal 化したときは、当該 task の
staging を同一遷移で冪等に cleanup する (**遷移内の順序は terminal 状態の耐久化 (done は manifest
一括確定の耐久化) が先、cleanup が後** — 逆順だと crash 時に公開元 bytes を失う。crash 時は遷移の
回復 replay で cleanup も再実行される。
非 terminal task の staging は `kio status` に表示し、prune-orphans の blocker として可視化する一方、
terminal 化済み task の残存 root は cleanup 失敗の残骸として prune-orphans の削除対象 —
[10-operations.md §7.5.1](10-operations.md))** — manifest には `pending`
という unit 状態は存在しない ([03-data-model.md §2.1](03-data-model.md) の遷移は failed → done のみ)。
**同一 root 名の残存時の前置回復**: 同一 `(raw64, tool64, adapter_kind)` の staging root が既に
存在する状態で新しい task を開始する場合、root 公開 (atomic rename) の**前**に旧 root の回復を、
**呼び出し元コマンドが既に保持する `.kio/.lock` の同一 critical section 内で**完了する
(04 §5.8 と同水準 — 別 lock の再取得はしない) — 対応 task が terminal なら cleanup を完遂してから公開し、非 terminal なら
新 task を開始せず当該 task を再開する。root 公開の rename は**既存 root 名への上書きをしない**
(no-replace — 新旧世代の bytes 混在を防ぐ。世代識別は path に載せない — [03-data-model.md §2](03-data-model.md) の配置は不変)。

**retry の合成規則**: staging の完了済み bytes は凍結する。retry 応答は**全 unit を含んでよい**
(未完了 unit のみへの絞り込みは任意の転送最適化 — Adapter は staging の内容を知り得ないため
Kio は要求しない)。**凍結保全と合成が適用されるのは transport 中断 (stream 失敗) からの resume に
限る** — 受け入れ検査 reject (contract violation — [04-pipeline.md §3.2](04-pipeline.md)) 起因の
再投入では staging を破棄して開始する (違反 unit を含み得る staging を first-instance-wins で
勝たせると、修正済み retry 応答が破棄され再違反が確定するため)。Kio が staging + retry 応答を合成する際、**まず生の retry 応答の各配列に V1 / V6 の配列内 unit_key 重複検査と配列間の排他検査
(pairwise disjoint — V1 の 4 集合 / V6 の 3 集合) を適用し** (staged-key の
再出現で重複・非排他が消える前に契約違反を検出する)、その後 **staging に確定済みの unit_key と
重複する応答 unit は黙って破棄する** (staging 側が first instance — first-instance-wins
([03-data-model.md §5](03-data-model.md)) と同型)。合成後の**完成集合に対して**受け入れ検査
(incremental は V1〜V6、full は full 契約) を適用してから一括公開する。

**staging の物理喪失** (プロセスクラッシュ・task cache 消失) 時は staging 全体を破棄し、未確定 unit
全体を再取得する — 完了済み bytes の凍結は転送量の最適化であり、正しさは全再取得で常に回復できる
(tasks.jsonl と同じ喪失許容 — [04-pipeline.md §5.7](04-pipeline.md))。

## 8.4 Capability 宣言なしの Adapter

`capabilities` に `incremental_update` を含まない Adapter は、Kio が **常に full モード** で呼ぶ。これは current capability contract の縮退であり、旧 store / object reader の後方互換ではない。

---

# 9. 再現性ポリシー

Adapter の完全な再実行決定性は要求しない。Kio が保証するのは:

```
raw_hash 不変                既存 artifact を尊重 (first-instance-wins)
raw_hash 変化                 新 artifact 候補を作る
explicit re-normalize         同 (raw_hash, tool_profile_hash) に対して gen+1 の新 normalized
                              instance を作る (kio reindex --regenerate、または prepared_hash 変化起因の
                              自動 gen+1 — 03-data-model.md §2.1 の例外)。旧 instance は
                              保全され、既存 commit / Evidence Pointer は旧 gen を参照し続ける
                              (03-data-model.md §2.1)
```

Markdown の content hash は持たない ([03-data-model.md §5](03-data-model.md))。同一 `(raw_hash, tool_profile_hash)` から複数回生成した結果が異なっても、**最初に確定したインスタンスを永続化** し、以後は再生成しない (first-instance-wins — 上記 explicit re-normalize の gen+1 は新 instance の追加であり、確定済み instance の置換・再生成ではない)。
