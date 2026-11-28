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

同梱 runtime が実行できる online target は `mistral_ocr_markdownize` と
`gemini_embedding_2` の built-in 実装に限定する。これらの role では `cmd` / `args` /
`url` による実行先の差し替えを受理せず、宣言された model と built-in target の一致を
その role を利用するコマンドの設定読取り時と実行直前に検証する。未使用 role は起動条件にしない
([操作別前提条件](10-operations.md#113-設定ファイル-schema-validation))。
利用者管理のローカル endpoint の送信境界は §3.1、embedding endpoint の設定は §5.3 に従う。

認証情報の保存規約:

private credential file の保護は [Kio 作成物の保護](10-operations.md#43-kio-created-object-boundary) に従う。
利用者が用意した tools.toml に平文 credential を置く場合も file 自体の owner-only 保護を必要とする。
Kio が作っていない親ディレクトリにはその要件を課さず、権限を変更しない。

```text
推奨:
1. 環境変数参照:       auth = "env:GEMINI_API_KEY"

許容 (非推奨):
2. tools.toml 直書き:  auth = "plain:<api_key>"
   - 条件を満たさない平文 credential は KIO-E-ADAPTER-TOOLS-PRIVATE-001
     (exit 2) で拒否する。既存の permission を自動変更しない。
   - そのコマンドが credential を利用する設定読取り時と使用直前に、1 MiB 上限の regular・single-link・
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

Kio は `deterministic_library` の Prepare / Markdownize Adapter を同梱する。対象: plain text / Markdown、PDF text layer 抽出。OCR・レイアウト解析・画像理解は行わない。**出力は単純な passthrough ではなく、Normalized Markdown v1 (§5.2.1) への決定的正規化**である — 少なくとも Setext 見出し → ATX 変換・生 HTML block の fenced text 化・改行 / 空白 / fence の正規化を行う (§5.2.1 準拠が受け入れ検査 V5 の通過条件のため、passthrough では通常の Markdown (Setext 等) がオフライン基線 index で全滅する)。

PDF text layer は FlateDecode を有界に展開し、object stream 内の辞書、ページごとの font resource、
subset glyph の ToUnicode 対応を扱う。有効な xref に従ったページ順で抽出する。未対応 filter や
必要な Unicode mapping がない範囲は空抽出とし、OCR の対象にできる。適用する Prepare / Markdownize の
profile はそれぞれ 1.4.0 / 1.5.0 とし、抽出意味論の変更を profile identity で区別する。
PDF はページ単位で有界に扱い、256 pages / 8,192 objects を文書全体の固定拒否条件にしない。
malformed object stream、循環参照、不正 offset/length は `contract_violation` とし、資源予算超過と区別する。
これらの失敗で弱い parser や有料 OCR への無断切替を行わない。資源 tier は §5.1 に従う。

- online Adapter が未設定または network 未承認のとき、Markdownize タスクは同梱 deterministic Adapter で実行する (タスクを止めない)。embedding の宣言がない場合は EG2 が既定であり、未準備なら理由を表示して text fallback する。明示選択した HTTP embedding が未承認の場合は EG2 に切り替えず、auto / hybrid は全文検索に縮退し、明示 vector / image / online はエラーとする ([05-runtime.md §1](05-runtime.md))
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

外部ネットワーク opt-in の単位・成立・寿命（loopback は §3.1）:

```text
単位: 外部ネットワーク送信は scope と Adapter の完全な paired identity。
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
  read-only で表示する。詳細な CLI 構文は [06-cli-spec.md §1](06-cli-spec.md) に従う。
```

`--online` は temporary opt-in でも grant 作成手段でもない。`index --online` は、
選択した configured Adapter 宛先の少なくとも一つに有効な承認があることを preflight で要求する。
外部宛先は本節の current scope/device paired grant、loopback は §3.1 の device approval を使う。
承認済み loopback だけの構成もこの preflight を満たすが、
各 Adapter の dispatch と query embedding は、宛先に応じて本節／§3.1 の承認を要求する。`--online` は revoke、identity
不一致、または policy 拒否を修復しない。

`--offline` は当該 invocation の外部ネットワークを禁止する。provider の submit / poll、
output download、upload cleanup、model download も外部宛先なら禁止する。
§3.1 の承認済み loopback Adapter と native 実行は許可し、offline watch にも同じ規則を適用する。
offline は承認を作成しない。index、batch resume/retry、reindex、repair rebuild-db、search と
同等の送信経路すべてで admission / dispatch の双方に適用する。

既定 local EG2 の準備は [10-operations.md §5.1](10-operations.md) に従う。
FTS baseline はモデルや API key に依存しない。

## 3.1 ローカル窓の送信境界

Embedding、Markdownize OCR、Rerank のローカル窓は、`http://` または `https://` の
loopback リテラルと 0 以外の明示ポートを要求する。host は `127.0.0.1`、`localhost`、`[::1]` とのリテラル完全一致とし、
名前解決結果による許可、userinfo、query、redirect、proxy を許可しない。パスは許可する。
違反は `KIO-E-LOCAL-PEER-URL-001`。設定検証と dispatch 直前に同じ条件を検査する。

loopback endpoint は端末で一度だけ明示承認する。device-private record は
scheme / literal host / port / path の完全な destination、credential binding、HTTPS の
trust generation/digest に結び付ける。scope/membership、tool/profile と operation を通常の
送信承認キーに含めず、同じ端末の別 scope / profile / 用途で同一 endpoint を再確認しない。
endpoint / credential / trust identity の変更・revoke 後は
再承認が必要で、admission と dispatch 直前に現行 record を検査する。scope からの trust / grant
import は認めない。承認・status は宛先、用途、送る内容の種類、端末内の対象 scope に適用される
ことを表示する。HTTP peer は未認証であることを明示する。proxy / redirect 禁止と URL 完全一致を
維持する。config の存在や auth 省略を承認にしない。

通常 loopback 承認は device-private な状態として保持する。未完了の承認を dispatch に使わず、
`approve --resume <grant_id>` は一致する endpoint identity と元の確認内容だけを再開できる。
中断後も確認されていない承認を active と推測せず、成功済み承認または未完了を一意に表示する。
revoke 完了後は後続 admission / dispatch が拒否され、送信済み bytes は取り消せない。
通常 index/watch は承認を補完しない。status は scope がなくても端末の承認・中断・失効を表示し、
破損・不一致を active と推測しない。

secret hold と必要な scope/device の secret-send paired grant は別に維持し、端末の通常送信承認で
秘密を解放しない。HTTPS は device-local の登録済み managed CA snapshot だけを信頼し、
送信前に証明書の有効性と名前を検証する。認証エラーは permanent、接続拒否は transport error とする。
HTTP に CA を要求しない。CA の登録・回転・失効・status は scope に依存しない。登録は CA の
device-private な snapshot を作成する。中断した登録の再開も同じ契約に従う。
登録済み CA は明示的な rotation でだけ置き換えられ、その際に generation を進める。
承認は active snapshot の実体と generation・digest に結び付く。選択された HTTPS 送信は trust-store lock の下で
active な CA の実体を固定・検証し、dispatch までその lock を保持する。この区間で rotation・revoke は完了できない。
回転・失効の後に古い承認で送信できず、登録済み CA を失効するとその状態は再起動後も
保持される。managed snapshot は [Kio 作成物の保護](10-operations.md#43-kio-created-object-boundary)
に従い、承認情報に CA の元パスや証明書本文を保存しない。`ca_pem_path` による直接指定は拒否する。

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
                         normalized task の完了を意味しない。同期の有料 incremental を送信した後は
                         自動 Full 再送を行わず result_unknown として明示承認を待つ。Batch の提供元は
                         制御応答を返さない (04 §5.8)。request outcome の区別は cost_ledger 側
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

**Office の準備 (DOCX / PPTX)** は、実行範囲を制限した LibreOffice の PDF 変換を使用する。
変換 PDF の `/CreationDate`・`/ModDate`・`/ID` 等の揮発 metadata は、xref offset を保つ同長の固定値に
正規化してから hash する。DOCX は PDF のページを `page:N`、PPTX は一スライド一ページの対応で
`slide:N` として列挙する。選択した renderer と変換条件は
[準備 profile](03-data-model.md#21-normalized-instance-と全文-view) に結び付ける。同じ出力 bytes でも、
準備 profile が変われば以前の unit をそのまま再利用しない。renderer が利用できないファイルは、
理由を `index_status` に表示し、online タスクを積まない。実変換の失敗は `contract_violation` とし、
同一入力の再試行は一回までとする。[04 の retry 契約](04-pipeline.md#53-エラー種別と-retry-budget) に従う。
音声の変換は対象外である。

renderer は `KIO_OFFICE_CONVERTER` で指定できる。未指定時は PATH の `soffice` を調べる。
配布元名、固定 install prefix、最低 version の文字列だけで可否を決めない。
実 executable identity と必要 runtime resource を特定・検証し、同じ実行条件で時間・資源を制限した実変換 probe で
headless PDF 出力 capability を確認する。名称・version・配布物 digest は provenance と診断に残す。
索引・処理中は変換する形式だけを probe する。明示的な診断コマンド `kio status` は DOCX と PPTX を
個別に probe し、失敗した形式と理由を表示する。
`kio search` は renderer の解決や probe を行わず、`index_status.office_conversion_unavailable` に
現在の公開済み HEAD に対応するファイル別の記録を返す。現在の renderer の利用可否と理由は
`kio status` で確認する。利用不能時は同コマンドの `office_readiness` に理由を表示する。
同じ索引実行内では検証済み renderer 実体の形式別 probe 結果（成功・失敗）を再利用し、準備と
タスク計画に使う。一方の形式の probe 失敗は他方の形式の可否に影響しない。実変換時の runtime 検証は各入力で行う。索引時に renderer が利用不能だったファイルは `kio status` の
`unsupported_inputs` にも表示する。これらのファイル別記録の `readiness_reason` は、索引時の probe の理由を
UTF-8 の文字境界で最大 1024 バイトまで保持する。変換後の PDF に抽出できるテキストがなく、OCR タスクも
計画できないファイルには `office_conversion_without_local_text` を記録する。
macOS は app bundle の Apple 発行コード署名と seal を全 architecture・nested code について検証し、
署名された bundle の主実行ファイルを起動する。PATH に shell wrapper がある場合は
`KIO_OFFICE_CONVERTER` に bundle 内の実行ファイルを指定できる。特定の Team ID や bundle ID の
指定、追加の notarization ticket 照会は行わない。Windows は `program/soffice.exe` と同じ
`program/` 内に `soffice.bin`・`fundamental.ini` があり、隣に `share/` がある配置を検証する。
Windows は launcher と renderer 本体を PE image として確認し、`program/` 直下のすべての PE image に
有効な Authenticode 署名を要求する。`program/` の下位ディレクトリの PE image は、署名があれば
実ファイル上で検証し、署名の破損や信頼エラーがあれば拒否する。署名がないことを確認できた場合は
許容する。配置内の `program/` 外の PE image には引き続き有効な署名を要求する。
trust 判定はローカル情報で行い、publisher 名やインストーラの署名は判定に使わない。
両 OS とも検証した runtime の配置と実体を probe・変換に結び付ける。macOS の bundle 内のリンクは
bundle の内側に閉じる。Windows は署名を持たない下位の PE image も配置全体の同一性検証に含め、
配置内の reparse point と複数の hard link を持つファイルを拒否する。
Windows の Office renderer は AppContainer を使わず、専用の Job で readiness probe と
実変換を実行する（D-20261122-02）。Office は呼出し元ユーザーの権限でファイル、レジストリ、IPC、
ネットワークにアクセスできる。Job はそれらのアクセスを制限しない。この実行条件を Windows Office の
準備 profile に結び付ける。
Windows は、GUI launcher の `--version` がコンソール入力を待つため、別プロセスでの version 照会を省略し、
provenance の version を `unknown` とする。
version 応答の形式差だけを「未導入」と扱わず、検出済みだが利用不能なら具体的な理由を表示する。
CI の配布物 SHA pin は再現性のため維持し、製品の一般的な版許可リストにはしない。

実行コードと必要 runtime path の置換防止・trust は storage の祖先 ACL 契約とは別に維持する。
Linux の distro / TDF / portable の配置は実際に必要な read-only runtime root の検証で受け入れ、
固定 `/opt/libreoffice<major>.<minor>` と sibling 名だけで決めない。runtime 内の symlink は
解決した対象を同じ許可済み runtime closure に固定できる場合だけ許可し、任意の host path へ
読み取り権限を広げない。Linux の network / unrelated private files / write の隔離を保った状態で probe を実行する。
Debian・Ubuntu のパッケージと標準の `/opt/libreoffice<major>.<minor>` に配置された TDF の DEB は、
構成済みの dpkg 台帳で所属と内容を照合する。この配置では `SHA256SUMS` があっても dpkg の照合を必須とする。
portable では、信頼できる配布物のファイル一覧を展開先直下の `SHA256SUMS` として用意する。
各行は SHA-256、空白 2 字、展開先からの相対パスとする。symlink は SHA-256 に `link:` を付け、
リンク先のパス文字列（末尾改行なし）をハッシュする。空ディレクトリは `dir`、空白 2 字、相対パスで指定できる。
対象は `program`、`share`、`presets` の中で、一覧にないファイルが混在する配置は拒否する。
相対パスは最大 1,024 バイト・32 要素、暗黙の親を含めて最大 32,768 エントリとする。
一覧は信頼できる配布物から作成し、現在の展開ツリーを自動的に承認するためには使わない。

renderer scratch は [Kio 作成物の保護](10-operations.md#43-kio-created-object-boundary) に従う。

変換には呼出しごとの private scratch と LibreOffice profile を用意し、環境変数は明示した値だけを渡す。
Linux と macOS は関連のない private file とネットワークへのアクセスを隔離し、隔離できなければ変換を拒否する。
Office 入力は 100 MiB、PDF 出力は 250 MiB とする。変換の wall-clock と CPU 時間の上限は各 300 秒、
対応環境のアドレス空間・Job メモリの上限は 2 GiB とする。scratch 増加の監視はカーネルの disk quota を
提供しない。macOS の scratch 監視では、`cache/fontconfig` にある Fontconfig の互換キャッシュ名が
同じディレクトリにある同一キャッシュの通常ファイルを指す場合、リンクをたどらず、実ファイルのサイズを数える。
変換 PDF は保持済みの出力ディレクトリからリンクをたどらずに読む。終了時はプロセスツリーを後始末する。

Windows では renderer を `CREATE_NO_WINDOW` 付きで起動し、Job に最大 8 プロセス、2 GiB のメモリ、
300 秒の CPU 時間の上限を適用する。標準入出力のハンドルだけを渡し、stdout と stderr の採取量を制限する。
入出力の CLI 引数は invocation の scratch を基準にした名前にし、PDF は保持済みの出力ディレクトリから読む。
起動ごとに `-env:UNO_SHARED_PACKAGES_CACHE` へ private scratch 内の空の `cache` の file URL を渡す。
変換ではこのキャッシュを共有拡張の検索先として使う。インストール先の `share/uno_packages/cache` は配置の同一性検証に含める。
Job の全プロセスが終了した後の最終 scratch 検証を
通った場合だけ出力を確定する。Job の監視・pipe drain・最終検査はすべて元の実行期限に含む。
失敗時は Job 全体の終了を要求し、後始末の確認は残り時間内に限る。timeout 時点で終了完了は保証しない。
Job の close 時にも全プロセスを終了させる。

macOS では fork を禁止し、物理メモリの使用量を 50 ms ごとに観測して 2 GiB の閾値を適用する。
超過または観測不能は timeout と同じ終了・回収条件に従う。この観測値は瞬間的なカーネル上限を表さない。
UNO service catalog は有界な private copy を使う。構造上有効な独立した MacSpellChecker の定義が
一件だけあればその定義だけを除去し、他の bytes を保持する。該当定義が存在しない構成では、隔離内での
実変換 probe が成功すれば未変更の private copy を使える。構造不正や曖昧な一致は拒否する。
呼出しごとに catalog の元の実体を再検証する。

**Office package preflight (DOCX / PPTX)**: LibreOffice に渡す前に raw ZIP を最大 100 MiB、central-directory を最大 4,096 entry、各 member を最大 32 MiB、全 member の declared aggregate を最大 256 MiB として検証する。必要な `[Content_Types].xml`、`_rels/.rels`、main XML の読取り合計は 6 MiB に制限し、CRC を検証して読む。encrypted / split ZIP、ZIP64、重複・非 UTF-8・unsafe part name、非 Stored/Deflated member は拒否する。XML は UTF-8、nesting 256、attribute 1,024 を上限とし、DTD/entity declaration を受けない。DOCX / PPTX ごとに genuine な MIME override、内部 root relationship、main part と namespace を検証する。preflight failure は renderer を起動せず contract violation とする。

**Linux renderer の資源境界 (v1 必須契約)**: 変換ごとに systemd user scope と cgroup v2 を
使用し、本文を処理する前に実際の所属と制限値を確認する。CPU / memory / pids controller、
`cgroup.kill`、安全な descriptor 相対解決、利用者の systemd manager が必須であり、
不足時は変換を拒否する。製品が sudo を呼び出したり、ホストの権限設定を変更したりしない。
native Actions の受入対象は Ubuntu 24.04 / systemd 255 である。
Linux desktop の対応条件は、この user manager と delegated cgroup v2 の資源制御、および既存の
filesystem/network 隔離を実検査で満たす構成とする。失敗時は `systemd user manager`、controller 名、
`cgroup.kill`、descriptor 解決、sandbox helper のどれが不足したかを表示し、単に renderer missing と
しない。隔離不能時も text/Markdown、PDF text layer、XLSX 直接抽出、既存保存内容の検索など
利用可能な経路を案内する。sudo・ホスト設定変更や隔離なしへの暗黙縮退は行わない。

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
この契約の実装・試験状況は [v1 実装記録](../tasks/v1-plan.md) に記録し、
前提検査の成功だけで実変換や native Actions の合格とは扱わない。

**XLSX の unit 化**は PDF 変換を経ず、sheet / row の構造を保持する決定論的なローカル抽出で行う。
row / sheet 単位で有界 streaming し、256 sheets / 50,000 rows / 1,024 columns /
1,000,000 padded cells を workbook 全体の固定拒否条件にしない。疎な列の padding を常駐展開せず、
共有文字列の参照・Markdown escape も資源予算へ数える。資源 tier は下記に従う。
ZIP central/local header の name、size、CRC の一致と CRC-checked read、unsafe/重複 part name、
encryption、split ZIP、ZIP64、非 UTF-8 part name、Stored/Deflated 以外の圧縮方式、
XML DTD/entity declaration・malformed 構造は拒否する。XML は UTF-8、nesting 256、
要素あたり attribute 1,024 以下を受理する。`[Content_Types].xml`、内部 root relationship、
workbook main part と worksheet relationship は XLSX の MIME / namespace / 対象 part と一致し、
外部 relationship を取得しない。必要 part の欠落や参照の不整合を空成功にしない。
これが XLSX ZIP/package 受入条件の正本であり、DOCX/PPTX の preflight 容量値は流用しない。
不正 ZIP/XML と上記の不受理 variant は `KIO-E-PREPARE-XLSX-EXTRACT-001`、
容量・work 予算超過は [04 §5.3](04-pipeline.md#53-エラー種別と-retry-budget) の
`resource_limit` と区別し、部分抽出を完成済み workbook として公開しない。
preview は完了 sheet/row、未処理範囲とその理由を表示する。worksheet 内では解析済みの行と
出力済みの行を区別し、判明している未処理開始行を元の行番号で示す。再開時も元 sheet/row identity
を保持し、完了シートの work と出力サイズも、その実行で選んだ tier の検査に含める。

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
  半端に配線せず、抽出器が**件数を報告して穴を可視に保つ** (`XlsxWorkbook::media_paths`)。

### PDF / XLSX の抽出資源 tier

`index` / `reindex` / `repair rebuild-db` の `--resource-tier standard|large` はローカル抽出の
invocation 限定の資源選択で、省略時は standard。永続的な安全確認無効化設定は作らない。

| 予算 | standard | large |
|---|---:|---:|
| 同時に保持する展開 object / stream / text / string table の合計 | 256 MiB | 1 GiB |
| parser work（object/reference 訪問、cell、展開 byte の 1 KiB 単位を加算）/file | 10,000,000 | 100,000,000 |
| 1 unit の出力 text | 16 MiB | 64 MiB |
| XLSX raw ZIP / file | 100 MiB | 1 GiB |
| XLSX 1 member の展開 bytes | 64 MiB | 256 MiB |
| XLSX 全 member の declared / 実読取り展開 bytes 合計 / file | 256 MiB | 4 GiB |

PDF の parser work には、object / reference の訪問、入力の走査、実際に展開した bytes を数える。
展開 bytes はファイル全体で累積し、累計を 1 KiB 単位へ切り上げるため、展開の chunk 分割によって
消費量は変わらない。可変長バッファは保持する容量を会計する。content stream、CMap、出力 text は
ページごとに解放する。圧縮 object の索引は member の位置を持ち、展開の作業領域には最大 1 コンテナを保持する。

XLSX の容量値は選択 tier に対応し、declared size と実読取りの両方に適用する。
member / aggregate の容量枠は streaming で消費する総量であり、同時保持 memory 枠の拡張ではない。
共有文字列・escape・出力を含む保持 bytes、parser work、unit text、ZIP 容量の全枠を同時に満たす。
全て有限の checked counter とし、割当・展開前に予算を検査する。メモリ予算は allocator の
全 RSS の保証ではなく parser が保持する bytes の上限である。page / sheet 出力は順に処理し、
合計が 64 KiB を超えた時点で、それまでの出力と以降の出力を private stage に保存する
（[04 §5.3](04-pipeline.md#53-エラー種別と-retry-budget)）。文書全体の展開 bytes は同時保持しない。
旧固定限界を超える正常 fixture が処理できることと、不正構造・展開爆弾・循環が有限 work で
止まることの両方を受入条件とする。
未完了の場合は処理済み範囲・未処理範囲・予算と再実行方法を表示し、cell / page を黙って捨てない。
PDF のページ総数が確定している場合は、予算情報の `page_progress` に総数、抽出済みの末尾ページ、
次の抽出ページ、再実行方法を含める。stage が作成された実行では、耐久化済みの末尾ページも
含める。末尾が 0 の範囲は空である。通常の `index` の保留出力にもこの情報を表示する。
ストレージ不足では空き容量の確保、standard の抽出予算超過では large への変更を案内する。
large の unit text 超過では該当ページのテキスト量を減らし、その他の抽出予算超過では PDF の
サイズや複雑さを抑えてから再実行する。
preview も同じ予算と受入検査に従い、有料 fallback や外部送信の承認を追加しない。

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
  failed_units                 [{ unit_key, error_kind, resource_limit? }] — 部分失敗の unit (error_kind は
                               [04-pipeline.md §5.3](04-pipeline.md) の閉 enum。04 §3.2 V1 の被覆に
                               含める (full は V6 の被覆)。resource_limit の共通記述は 04 §5.3。persist されず manifest 側で failed へ遷移
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

- 同期と Batch の OCR リクエストは `include_blocks: false` を明示する。段落ブロックは Kio の出力に使わず、profile hash の入力にも含めない。
- 表は Markdown 本文に inline で保持する (`table_format=null` 相当)。独立 table object は作らない。
- 文書内 embedded image は抽出して image object ([03-data-model.md §2](03-data-model.md)) として保存し、Markdown 内の参照は `kio://<scope_id>/object/image/<image_hash>` に置換する ([08-evidence-pointer-spec.md §2.3](08-evidence-pointer-spec.md))。
- bbox / page / confidence score は unit metadata に記録する。**Evidence Pointer schema には含めない**。
- 図中テキストは `[markdownize] bbox_annotation` が有効な場合（既定）、説明・書き起こしを
  unit metadata に記録し、参照画像の近くに検索可能テキストとして反映する。設定は `tool_profile_hash` に含める。
  Mistral の bbox annotation の追加費用は OCR 単価に反映する。生成 LLM による二次 fallback は対象外とする。
  - wire は次の exact `bbox_annotation_format` JSON Schema 1 個を使い、説明/書き起こし指示は schema
    field description に固定する (bbox 専用 prompt parameter は使わない)。各 `pages[].images[]` の
    `image_annotation` は `short_description` / `transcribed_text` の厳密 JSON とする。JCS byte 列の
    sha256 `sha256:79443071238e373b41e34818e91d52ea69d2bba70a30169cda2cf98bfd8bea76`
    を enabled profile の `prompt_template_hash` とする。

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

## 5.2.1 Normalized Markdown 形式

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
対象選択・Task・論理 job・実行記録・member 受入・再試行・公開は
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
[03-data-model.md §8.1](03-data-model.md) に従う。画像 payload は 16 MiB、各辺 8,192 pixel、
総画素数 1,600 万、decode の割当 64 MiB を上限とする。OCR 後に保持する GIF は最大 32 frame、
全 frame で 1,600 万 pixel / RGBA 64 MiB までとする。
HTTP の画像 embedding は一送信につき一画像とし、有界メモリで処理する。
無料のローカル HTTP でも、各送信に現在の policy と宛先承認、必要な別の secret-send 承認を要求する。
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
| 窓 | 全文を試す。固定 LiteRT-LM v0.19.1 C API の embedding 計算が code=3 を返し、診断が `INVALID_ARGUMENT: ERROR: [third_party/odml/litert_lm/runtime/core/embedding_engine_impl.cc:<行番号>]`、LF、`└ ` に続く `Input sequence length (N) exceeds maximum supported signature length (2048).`（N>2048）に完全一致する場合だけ InputOverflow として二分する。他の native error は分割理由にしない |
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

端末の `tools.toml` で設定する。次は messages 形式の例である。二つの SHA-256 は利用する重みと、
サーバーが使う chat template の値に置き換える。

```toml
[embedding.local_embedding]
kind = "offline_api"
url = "http://127.0.0.1:8000"
wire_format = "messages"
model = "my-embedding-model"
model_version_pin = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
chat_template_hash = "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
instruction = ""
```

`wire_format` は `input` または `messages` を明示する。model・重み pin・chat template の SHA-256・
instruction（空文字列を含む）も必須で、wire 形式とともに意味的 profile に固定する。
template を使わないサーバーでは空の template の SHA-256 を記録する。重みと template の pin は
利用者が管理するサーバーの構成を宣言する値である。同一 profile で input/messages を混用しない。
`messages` で空でない instruction は system message として送る。
`input` では instruction の文字列を本文の直前に連結し、必要な区切りも設定値に含める。

リクエストの `dimensions` は 768 とする。利用するモデルが先頭次元の切り詰めをサポートする場合に
のみ `truncate_dimensions = true` を設定できる。この設定も profile に固定する。余剰次元は
この設定時だけ切り詰めて L2 正規化する。寸法不足と、設定なしでの超過は契約違反として拒否する。
`kio adapter approve local_embedding --preview` で送信内容を確認し、
`kio adapter approve local_embedding --yes` で宛先を承認する。
応答の `data` は 1 要素で、入力に対応する整数の `index: 0` を要求する。
HTTP 4xx の要求拒否は `invalid_input` とする。408 と 429 には通信障害と rate limit の再試行規則を適用する。
窓は同じ Task・無料 local execution state・member 受入・回復契約を使う。Kio は endpoint server を起動・管理しない。
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

config (`~/.config/kio/tools.toml`) では `mistral-ocr-latest` のような可変 alias を指定できる。Adapter はモデル一覧で設定値と完全一致する `id` を探し、その行の `id` と `aliases` を候補とする。`-latest` で終わる名前と、`deprecation` が非 null の候補は除外する。設定 alias 自体が廃止済みの場合や、候補の行が一覧にない場合は契約違反とする。
残った候補のうち、他の全候補をハイフン区切りの接頭辞として持つ一意の名前を選ぶ（例: `mistral-ocr-4` → `mistral-ocr-4-1`）。候補が空、または選択が曖昧な場合は契約違反とする。
`-latest` で終わらない設定値は一覧照会せずに使う。同期リクエストと Batch job の作成には解決済みの pin を使い、OCR 処理結果と `tool_profile_hash` の `model_version_pin` にその pin を記録する ([03-data-model.md §5.1](03-data-model.md))。Batch 回収では帰属を照合した job の `model` から送信時の pin を取得する。応答本文に `model` がある場合は pin と完全一致が必要とする。Mistral OCR の Batch 結果では、公式出力例にある応答表記 `<pin>-completion` も同じ pin として扱う。それ以外の不一致は、一覧にある alias を含めて契約違反とする。モデル更新は `tool_changed` として扱われ、再 Markdownize は first-instance-wins / gen の既存機構 (§9) に乗る。

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

設計上は `[adapter.policy.<execution_mode>]` で `online_api`、`offline_api`、`deterministic_library`
ごとの値（`timeout_seconds`、`max_input_bytes` など）を上書きでき、未指定のキーは共通設定を継承する。
適用対象とタイミングは変わらず、上限は AdapterRun 一回（request または job）ごとに適用する。
`offline_api` 専用の既定値はローカル OCR の実測で決める。

現行実装で有効な上書きは `[adapter.policy.offline_api]` の `timeout_seconds` だけである。
省略時は共通の 300 秒を継承する。共通 `timeout_seconds` の 300 以外の値、
`online_api` と `deterministic_library` の sub-table は `KIO-E-CONFIG-NOT-IMPLEMENTED-001` で拒否する。

Markdownize / Embedding runtime は同梱 target だけを実行し、任意コマンドの実行経路を持たない。

ログに残してよいもの:

```
task_id, adapter_id, tool_profile_hash, execution_mode, scope_id
input_raw_hash, output_hash
status, error_code, error_category, retry_after_ms
network_consent (宛先に応じた current grant identity — HTTP dispatch を伴った実行のみ)
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
1. Kio が実行する Adapter は Kio 同梱の built-in target のみを trusted code として扱う。

2. [adapter.policy] は「Kio 側の入力制御 + 事後監査」の規約であり、
   sandbox による強制保証ではない。
   - `max_input_bytes` は **AdapterRun 1 回の入力 (prepared input の canonical bytes 合計)** に
     適用する (**AdapterRun = 1 回の Adapter 呼出 = 1 request / job** — §4・§5.5 の課金報告と同一
     単位。task 全体の総量上限ではない — 総量は budget cap 側が律する) — 超過は送信前に当該 task を
     terminal failed (invalid_input・非再試行) とし、送信しない (課金なし)
     Office の変換前に原本を読み込む処理は、ローカルの standard メモリ予算内で行う。
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
                              instance を作る (kio reindex --regenerate、または準備の出力・profile の変化による
                              自動 gen+1 — 03-data-model.md §2.1 の例外)。旧 instance は
                              保全され、既存 commit / Evidence Pointer は旧 gen を参照し続ける
                              (03-data-model.md §2.1)
```

Markdown の content hash は持たない ([03-data-model.md §5](03-data-model.md))。同一 `(raw_hash, tool_profile_hash)` から複数回生成した結果が異なっても、**最初に確定したインスタンスを永続化** し、以後は再生成しない (first-instance-wins — 上記 explicit re-normalize の gen+1 は新 instance の追加であり、確定済み instance の置換・再生成ではない)。
