# 07 Adapter Spec

Adapter (Prepare / Markdownize / Embedding / Rerank) の trait 契約 + 実行形態 + Markdown incremental プロンプト規約。

> 関連: [03-data-model.md §5](03-data-model.md) (`tool_profile_hash` 計算規約) / [04-pipeline.md §3](04-pipeline.md) (incremental Markdownize) / [06-cli-spec.md §9](06-cli-spec.md) (JSON output / Adapter 境界)

---

# 1. 基本方針

Prepare / Markdownize / Embedding / Rerank は Kio core に含めず、**Adapter に委譲** する。OCR は Markdownize Adapter の **内部能力 (capability)** として扱う。Embedding は Text / Image を分離せず、**一つの Embedding 契約** に統合する。画像 capability の有無は profile に宣言する。

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
native_library           Kio 管理 worker 内の native library (ネットワーク不使用)
offline_api              利用者管理のローカル LLM / embedding endpoint
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

- online Adapter が未設定または network 未承認のとき、Markdownize タスクは同梱 deterministic Adapter で実行する (タスクを止めない)。Embedding はモデル準備済みなら既定の EG2 を使い、未準備なら理由を表示して text fallback する ([05-runtime.md §1](05-runtime.md))
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
loopback local Adapter、provider の poll、output download、upload cleanup も対象である。
未実行の task はこの invocation で送信せず、HTTP 回復も行わない。適用対象は index、batch
resume/retry、reindex、repair rebuild-db、search、および HTTP Adapter を使い得る同等の経路である。

既定 local EG2 の準備は [10-operations.md §5.1](10-operations.md) に従う。HTTP Adapter は
同梱されていても paired grant 成立前に内容を送信しない。FTS baseline はモデルや API key に依存しない。

## 3.1 ローカル窓の送信境界

Embedding、Markdownize OCR、Rerank のローカル窓は、`http://` または `https://` の
loopback リテラルと 0 以外の明示ポートを要求する。host は `127.0.0.1`、`localhost`、`[::1]` とのリテラル完全一致とし、
名前解決結果による許可、userinfo、query、redirect、proxy を許可しない。パスは許可する。
違反は `KIO-E-LOCAL-PEER-URL-001`。設定検証と dispatch 直前に同じ条件を検査する。

HTTP peer は認証されていないことを承認画面と status に表示する。HTTP でも scope/device の
exact paired grant、secrets hold と必要な secret-send grant、`--offline` の禁止は変わらない。
HTTPS では device-local の登録済み managed CA snapshot だけを信頼し、送信前に証明書の
有効性と名前を検証する。CA の登録・回転・失効・status と snapshot の寿命は
[14-v1-runtime-contracts.md](14-v1-runtime-contracts.md) の既存 trust 契約に従う。
`ca_pem_path` による直接指定は拒否する。`auth` の省略も paired grant を免除しない。

ローカル窓の接続先は device の tools 設定に置き、`cmd` / `args` は受理しない。
Markdownize OCR は `POST {url}/layout-parsing` を使う。Embedding の wire は §5.3 に従う。
EG2 はローカル窓ではなく native 実行であり、この HTTP/CA/送信 grant の対象外である。

---

# 4. 共通メタデータ

すべての Adapter は次を返す。Embedding の outcome/usage は §5.3 の型付き契約を使う:

```
AdapterProfile:
  adapter_kind          "prepare" | "markdownize" | "embedding" | ...
  adapter_id
  execution_mode        "online_api" | "offline_api" | "native_library" | "deterministic_library"
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
                        "tokens_in" | "image_tokens_in" | "tokens_out" の閉 enum — 拡張は spec 改訂。count = 非負整数。
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

### レーン別の価格解決

`tools.toml` の宣言単価は Standard 単価とする。Batch 対応 online Adapter の Batch 単価は
その 0.5 倍、realtime は Standard とする。予約見積りと usage 精算は同じレーン・同じ価格解決を使い、
Batch 単価を設定へ直接書いて二重に割り引かない。レーン選択は [03-data-model.md §11](03-data-model.md)。

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

> **実地検証済み (2026-07-03、Adapter 選定 — [09-mvp-scope.md §5](09-mvp-scope.md))**: 合成 fixture (複雑表・日本語・数式・埋め込み画像、4 ページ) を sync / Batch 両モードで検証: 表セル一致率 1.0 (17/17)、日本語 CER 0.0、画像抽出 1/1 (placeholder 形式も §5.2 想定どおり)、数式は LaTeX でテキスト化。単価は公称一致 (API $4 / Batch $2 per 1,000 pages)、Batch のジョブ往復は 4 ページで約 24 秒。ハーネスと実測ログは `experiments/ocr-verification`。

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

## 5.3 Embedding

text chunk、所有権を検証した image object、text query は同じ Adapter 契約を使う。
対象選択・Task・論理 job・物理台帳・member 受入・再試行・公開は
[04-pipeline.md §5](04-pipeline.md) の共通 runner が所有し、Adapter はそれらを分岐実装しない。
audio/video、PDF の直接 embedding、画像ファイルを query にする入力は対象外である。

```text
request:  profile, input_type (markdown_chunk | image_object | query), members [{id, text | image_bytes, mime?}]
outcome:  Completed { members: [{id, vector | failure_reason}], usage }
        | Pending { remote_handle }
usage:    Metered {usd | billable_units} | Estimated | Unmetered
```

ローカル実行は Completed のみを返し、remote handle や poll を捏造しない。
共通 descriptor は modality/MIME、入力・encoded bytes 上限、completion 能力、価格・usage 単位、
egress と paid authorization の要否を値で宣言する。native は bit 単位の OS 間決定性を約束しない。

### 入力構築

text の `chunk_filename_context_v1` は `{humanized filename}\n\n{chunk 本文}`。
humanized filename は basename stem の `-`/`_` を空白へ、ASCII camelCase 境界を空白へ変換し、
空白を畳み込む。英数字を含まない純記号名には prefix を付けない。
認証済み chunk CAS 本文から NFC 正規化 → NUL 除去 → Markdown escape/entity 解決の順で
検索用本文を作る。fenced block / inline code 内の escape は解決しない。FTS と同じ構築規則を使い、
SQLite の本文を送信元の正本にしない。query は filename を持たない。
context identity と reuse は [03-data-model.md §8.1](03-data-model.md) に従う。

画像は PNG/JPEG の元 CAS bytes と MIME を保つ。GIF/WebP は固定変換 `kio_image_png_v1` を使う。
frame 0 を full logical canvas に合成した透明 RGBA8 straight alpha とし、透明 pixel の RGB は 0、
resize/crop なし、WebP EXIF orientation 適用、ICC/gamma 色変換なしの PNG とする。
出力に可変 metadata/timestamp を含めず、固定 codec pin の同一入力は 3 OS で同じ SHA-256 を持つ。
元 CAS、image target、Evidence Pointer は差し替えない。receipt と入力 identity は
[03-data-model.md §8.1](03-data-model.md)、保護上限は
[14-v1-runtime-contracts.md](14-v1-runtime-contracts.md) の画像契約に従う。
MIME magic と宣言 MIME、bounded decode、元 hash、typed ownership を予約・dispatch 前に検証する。
GIF の変換は animation 全体の取り込み検証を迂回しない。独立 GIF の新規 ingest は追加しない。

### Gemini

| 項目 | 契約 |
| --- | --- |
| model / 出力 | `models/gemini-embedding-2` / 768 次元 / cosine / multimodal |
| text | 現行 filename context・query 入力・profile hash を維持。taskType/title/retrieval prefix を追加しない |
| image | 1 member = 1 image、`content.parts[].inlineData {mimeType,data}`。text と image を一つの member に混ぜない |
| sync | `POST /v1beta/models/gemini-embedding-2:batchEmbedContents`、`requests[]` の model/content/`embedContentConfig.outputDimensionality=768` |
| Batch | `:asyncBatchEmbedContent`、`batch {model,displayName,inputConfig:{requests:{requests:[{request,metadata:{key}}]}}}`。displayName に intent attribution、key に member 対応を固定 |
| 回収 | long-running operation の metadata/state、一覧の operations、`metadata.output.inlinedResponses.inlinedResponses[]` または `response.inlinedResponses.inlinedResponses[]` の metadata/key と response を検証。sync は入力順との対応を検証 |
| 上限 | 全 modality 合計 8,192 tokens/member。PNG/JPEG。sync/Batch の encoded body は base64/JSON 込み 16 MiB 以下（Kio の保守上限） |
| 分割・失敗 | job を bytes/member 上限の双方で分割。単体超過は理由付き Failed。自動 truncation、Files API、黙った resize、有料レーン変更による救済なし |
| 単価 USD/1M | Standard: text 0.20 / image 0.45、Batch: text 0.10 / image 0.225。単位は tokens_in / image_tokens_in |
| 予約 | member ごとに 8,192 × 当該 modality の token 単価を合算。image は Standard $0.0036864 / Batch $0.0018432 を予約。実際の画像 token 数を固定する意味ではない |
| usage | promptTokenDetails の TEXT/IMAGE を対応単位へ写像。total 整合、非負、未知/重複 modality、予約上界を検査。sync は call 合計、Batch は各 member の usage を扱うため text/image は別 job |

usage 欠落・不正・不完全な回収は予約額による Estimated 精算とし、請求一致とは表現しない。
sync 失敗を無料と推測しない。価格は意味的 profile の対象外。request body、画像 bytes、key はログへ出さない。
Batch の停止条件・明示解除と bounded attempt の正本は [04-pipeline.md §5.8](04-pipeline.md) に従う。

### EmbeddingGemma 2 (EG2、既定 local)

| 項目 | 契約 |
| --- | --- |
| runtime | 配布・対応 target・pin 検証は [10-operations.md §5.1](10-operations.md)。CPU text/vision、audio 不使用 |
| engine | Kio と同一 binary の管理 worker 内で native library を使う。外部 server、Python、証明書は不要。ネットワーク送信・listen なし |
| 入力・出力 | text/image/query、cap 2048、特殊 token と正規化有効、overflow ERROR、768 次元 / cosine / multimodal |
| prompt | query prefix は `task: search result | query: `。document は `title: none | text: ` に上記 filename context 付き本文を続ける固定 template とし、exact prompt bytes を profile に pin。空 prompt や別文面へ置換しない |
| 窓 | 全文を試し、code=3 かつ `Input sequence length (N) exceeds maximum supported signature length (2048).` の N>2048 だけを InputOverflow として二分。他の native error は分割理由にしない |
| 境界 | 本文 Unicode scalar 中央半分で段落空行 → 文末 `.。!！?？` → 文字境界。同距離は先の境界。全連続 byte 区間を重複・欠落なく覆い、各窓に同じ prompt/context。長い query も同じ規則、truncation なし |
| 集約 | 窓を L2 正規化 → 本文文字数で加重平均（prefix を除外）→ 再 L2 正規化。固定順 f64 集約、最終 f32。全窓成功後に一つの vector を返し、窓別 Task/保存を作らない |
| 有界性 | 共通 max_chars=6000 を維持。depth 32、本文 scalar 数由来の二分木 call 上限と時間上限。空/零 norm/不正値/分割不能は明示失敗。小窓の overflow も本文を切り捨てない |
| engine slots | device 全体で背景 indexing（2 threads、低優先度）と対話 query（CPU 数以下で最大4 threads、通常優先度）の2枠。query は indexing を待たない。第2 indexing は bounded wait 後 Paused(resource_busy) |
| 寿命 | command 内は再利用し、終了/cancel 時に worker と resident memory を解放。常駐 daemon なし。fresh CLI は毎回 load |
| 故障 | native abort、worker EOF、不正応答、timeout、cancel、memory limit は親 Kio を終了させず member を理由付き失敗にする。native diagnostic は有界・private に隔離し、通常出力は分類理由のみ |

local crash の再計算・0 USD 回復は [04-pipeline.md §5](04-pipeline.md) に従う。online へ自動送信しない。
モデル取得・保存・検証は [10-operations.md §5.1](10-operations.md)、CLI は [06-cli-spec.md](06-cli-spec.md)。

### 汎用ローカル endpoint 窓

モデル名を設定可能とし、次の二つの wire 形式を選ぶ。どちらも `POST {url}/v1/embeddings` を使う。

| wire | 入力 capability |
| --- | --- |
| OpenAI 互換 input | text のみ（Ollama / LM Studio 等）。image capability は宣言せず、image は Skipped(image_capability_unavailable) |
| vLLM chat-form messages | text と image。同 profile の text/query も messages を使う。image は data URI 付き user message、1 item/request |

wire 形式・model・重み pin・chat template/instruction を意味的 profile に固定する。
空 instruction も固定し、同一 profile で input/messages を混用しない。
出力は768次元を要求し、形式が許す場合だけ余剰次元を切り詰めて正規化し、そうでなければ明示拒否する。
窓は同じ Task・0 USD 台帳・member 受入・回復契約を使う。Kio は endpoint server を起動・管理しない。
HTTP/HTTPS の送信境界は §3.1 に従い、別の実行 pipeline や管理 server 選択肢は設けない。

## 5.4 Rerank (optional)

```
input:   query, candidate_result_ids, candidate_features
output:  reranked_result_ids, scores
metadata: profile_hash, searched_scopes, fallback_reason
```

Rerank Adapter は Kio の検索結果を再順位付けするだけで、**searched_scopes / fallback_reason を隠蔽してはならない**。

## 5.5 Batch 実行契約とプロバイダ採用条件

Batch モードを持つ online Adapter (Markdownize / Embedding) は、[04-pipeline.md §5.8](04-pipeline.md) の
2 相プロトコルが要求する次の操作を trait として公開する。Gemini は inline 専用のため upload/list_uploads/delete_upload を持たず、intent attribution は §5.3 の displayName を使う:

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

1. job 一覧照会 (または token による job 発見) が可能であること。upload を行う provider/path だけは
   upload/list_uploads/delete_upload も提供し、未記録 upload 残骸を発見・削除できること。
   Gemini inline のような upload なし経路に upload 操作の提供・一覧・掃除を要求しない。
2. job 作成 → 一覧可視化の遅延に上限があること (Kio の可視化猶予 既定 10 分以内)。upload を行う場合だけ
   upload → 一覧可視化にも同じ上限を適用し、upload() は upload_id を返し、list_uploads は pagination を
   提供すること（回復と終端 token 解放は [04-pipeline.md §5.8](04-pipeline.md)）。
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

Markdownize の Batch は 1 job = 1 task、custom_id = task の input_hash、job metadata は
`intent_token / scope_id / adapter_kind / input_hash / tool_profile_hash`、upload filename は
`kio-<intent_token>.jsonl` とする。レーンは設定・CLI で選択し、価格解決は §4 に従う。
期限切れで実費不明なら予約額の Estimated 精算、未完了 entry が非課金の FAILED/CANCELLED は 0 精算とする。

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
5. spec_version 不一致なら、Adapter は invalid_input として失敗する (`KIO-E-BATCH-INPUT-001`。汎用 `KIO-E-ADAPTER-CONTRACT-001` (retryable 1 回) と区別し、[04-pipeline.md §5.3](04-pipeline.md) の invalid_input 分類 = max_attempts 0 に対応させる)
6. 出力は Kio 側の受け入れ検査 (04-pipeline.md §3.2) を通過しなければ persist されない。
   違反は KIO-E-ADAPTER-CONTRACT-001 として reject され、**incremental capability 非互換の場合に
   限り** full に fallback する (spec_version 非互換は下記のとおり fallback しない)
```

`spec_version` の bump 規約は [10-operations.md §11.5](10-operations.md) を正とする。**full fallback (§8.4) が有効なのは incremental capability だけが非互換な場合に限る** — full request も同じ `spec_version` を含むため、spec_version 自体の非互換は full で呼び直しても同じ invalid_input を再生するだけである。この場合は invalid_input (`KIO-E-BATCH-INPUT-001`、§8.1 手順 5 — 非再試行) として当該 online Adapter のタスクを failed permanent (Adapter 更新が必要) にし、同梱 deterministic Adapter のベースライン (§2.1) は影響を受けず継続する。

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
