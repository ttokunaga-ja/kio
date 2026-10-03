# 03 Data Model

統合元: 旧 `research/git_kio.md` (CAS / DAG) + 旧 `research/kio.md` (.kio layout) + 旧 `research/hash.md` (identity) + 旧 `research/read_only.md` (write boundary)。いずれも正本ではなく、2026-07-18 に docs から撤去 (経緯は git 履歴で参照可)。

> 本書は現行 format / 実装契約を定める。v1 の製品到達要求と RC の未充足事項は
> [11-product-requirements.md](11-product-requirements.md) を正本とする。本書の詳細な RC 制約は、
> v1 の実装済み表明ではない。

---

# 1. 概念モデル — CAS + Snapshot DAG

Kio は Git inspired な content-addressed store と snapshot DAG を、ローカルファイル全体に拡張したアーカイブ。

```
Object 種別:
  raw          原本ファイルバイト列
  prepared     Markdownize 前の中間表現 (page image, sheet etc.)
  image        文書内 embedded image (Markdownize 時に抽出。type は予約済み、実装は Step 2
               [09-mvp-scope.md §3.1](09-mvp-scope.md))
  manifest     normalized instance manifest の確定版 (canonical JCS bytes — §2.1。tree の
               normalize.manifest_hash が指す)
  toollock     tool-lock.json の確定版 (canonical JCS bytes — §5.2。commit の tool_lock_hash が指す)
  normalized_unit_object  unit 単位の完全な NormalizedUnitObject (canonical JCS bytes の CAS)。
                          normalized の immutable な正本 (§2.1)
  chunk        normalized から見出し単位で切り出し
  embedding    chunk のベクトル表現
  tree         path → object_hash のスナップショット
  commit       tree + parents + metadata
```

raw / prepared / image / normalized_unit_object / chunk / embedding / manifest / toollock / tree / commit は **CAS object** として `objects/<type>/ab/cd/<digest64>` に保存。hash の算出は object 種別ごとに §8.1 で規定する: raw / prepared / image は**バイト列そのものの content hash**、normalized_unit_object / manifest / toollock / tree / commit は **canonical JCS JSON 保存バイト列の content hash**、chunk / embedding は **identity タプルから導出する identity hash**。`normalized_units/` の path-named instance は current working projection / cache として残してよいが、履歴の正本ではない (§2.1)。ファイル全文の normalized Markdown も unit を決定論的に結合した **view (再生成可能な cache)** であり、正本ではない。

# 2. .kio 物理レイアウト

```
.kio/
  .store-gate         init 時に全 OS で作る必須の空 regular file (恒久的な concurrency gate)
  HEAD
  config.toml         folder-scope の設定 (ignore, chunking, search, budget)
  scope.json          scope_id (init 時採番の ULID、以後不変) とこのフォルダ自身・子 .kio リンク
                      (旧称 folder.json は廃止)
  tool-lock.json      Adapter capability 記録 (cmd/url/auth は含めない)
  manifest.json       working/index state (永続的真実は tree/commit object)
  staging/            外部実行の streaming staging ([07-adapter-spec.md §8.3](07-adapter-spec.md))。
                      配置 = `staging/<raw64>.<tool64>.<adapter_kind>/`、各 root 直下に耐久
                      descriptor.json (scope_id / raw_hash / tool_profile_hash / adapter_kind)。
                      **root の公開 = private temp directory に descriptor ごと完書き → fsync →
                      root 名へ atomic rename (no-replace — 残存 root への上書き禁止、前置回復は
                      [07-adapter-spec.md §8.3](07-adapter-spec.md)) → 親 directory fsync ([04-pipeline.md §1.1](04-pipeline.md)
                      の primitive) — payload の書込みは公開後にのみ行う** (descriptor より先に
                      payload が存在する窓を作らない)。**purge / status / prune-orphans の
                      帰属列挙は descriptor の全走査が正本** (tasks.jsonl 非依存 — task 記録の
                      喪失許容 ([04-pipeline.md §1](04-pipeline.md)) と両立させる唯一の手段)。
                      **descriptor の無い root・path と不整合な root (descriptor の有無を問わない —
                      crash 残骸・旧 store)・terminal 化済み task の残存 root (cleanup 失敗の残骸 —
                      [07-adapter-spec.md §8.3](07-adapter-spec.md)) は fsck / status が表示し、
                      `--prune-orphans` の削除対象とする** (帰属不明の staging は
                      安全側 = 削除。対応 task 特定不能の root も回復判定 — 全世代の instance
                      manifest が terminal + 同 key の in-flight 不在、または確認付き in-flight
                      不在検証 — を経て削除可能 —
                      [10-operations.md §7.5.1](10-operations.md))
  objects/
    raw/ab/cd/<raw64>
    prepared/ab/cd/<prepared64>
    image/ab/cd/<image64>           # 文書内 embedded image (type 予約済み、実装 Step 2。
                                    # dir 名は §1 の objects/<type>/ 公式どおり type 名 (単数形) と一致させる。
                                    # media_type は unit metadata に記録)
    normalized_units/ab/cd/<raw64>.<tool64>.g<gen>/
      manifest.json                    # current working projection (最新版、§2.1)
      <unit_ref>.json                  # current mutable view/cache (unit_ref = base16(sha256(unit_key の UTF-8 バイト列))[0:16])
    normalized/ab/cd/<raw64>.<tool64>.g<gen>.md   # 全文 view (cache, 再生成可能)
    normalized_unit_objects/ab/cd/<unit-object64> # immutable NormalizedUnitObject (canonical JCS bytes、§2.1)
    manifests/ab/cd/<manifest64>    # manifest の immutable 確定版 (canonical JCS bytes、§2.1。
                                    # tree v2 の normalize.manifest_hash が指す — §8)
    toollocks/ab/cd/<toollock64>    # tool-lock の immutable 確定版 (canonical JCS bytes、§5.2。
                                    # commit の tool_lock_hash が指す)
    chunks/ab/cd/<chunk64>
    embeddings/ab/cd/<embedding64>
    trees/ab/cd/<tree64>
    commits/ab/cd/<commit64>
  refs/
    heads/main
    tags-v1/tag-<digest64>          # canonical: digest64 = sha256(NFC + simple case folding 後の論理 tag 名の UTF-8 バイト列)
    tags-v1/names.jsonl             # 論理 tag 名の truth (append-only ledger — 下記 tag 規則。
                                    #  leaf が tag- で始まらないため ref 列挙と衝突しない)
  tombstones/ab/cd/<raw64>      purge の tombstone lifecycle 記録 (raw_hash ごとの append-only events[] —
                                purged / retired。active 判定 = 末尾 event (marker 単独の規則 — 解決時は
                                08-evidence-pointer-spec.md §3.1 手順 5 の canonical 正本化を経る)。05-runtime.md §3.5。CAS object ではない)
  purge/epoch         purge の ABA barrier (単調カウンタ — 05-runtime.md §3.5。欠落 = 読取 fail-closed)
  purge/erase-receipts/ab/cd/<raw64>   erase receipt (non-public marker — events[] lifecycle。§4.1 の
                                truth・復旧不能。用途列挙の正本は 08-evidence-pointer-spec.md §4.2)
  tombstones/lifecycle-epoch    lifecycle 更新 (retire・再 purge) の単調カウンタ
                                (05-runtime.md §3.5 — 回転補完の検出源。event append ごとに +1)
  tasks.jsonl         batch タスクストア (04-pipeline.md §5.1。append-only の運用データ、SQLite 非採用。
                      terminal task の行の bounded compaction あり (task 状態 = 最新行。terminal task は
                      全行を落とし、非 terminal task は最新行のみ残す) — 04 §5.1)
  chunks.jsonl        chunk association ledger (**truth** — chunk object が持たない世代 association の正本。
                      作成行 = {chunk_id, chunking_config_hash, created_at, path}。
                      path = chunk 生成時点の path (SQLite chunks.raw_path の rebuild 入力)。
                      **publication event 行** = {event:"publication", chunk_id, chunking_config_hash,
                      introduction_commit} — すべての durable association は creation fsync 後に
                      対応 event を持つ。event は同じ `(chunk_id, chunking_config_hash)` creation
                      より後にだけ置ける。追加 introduction (incomparable な別枝での公開、
                      association の後発公開) も auto snapshot 時に append する。publication relation
                      (04 §4.1 cache) の rebuild 正本はこの ledger (digest は照合のみ)。append-only、
                      SQLite rebuild の入力 — 04-pipeline.md §4.1/§4.6、本書 §8)
  index/
    sqlite.db         FTS5 + sqlite-vec (query acceleration layer; 真実は objects/)
  logs/
    access.jsonl
```

`<raw64>` / `<tool64>` / `<digest64>` 等は、対応する論理 hash
`sha256:<64 lowercase hex>` から `sha256:` を除いた **64 文字の小文字 hex digest** である。
物理ファイル名とディレクトリ名にはこの digest-only 表現を使い、`:` を含めない。JSON、SQLite、
refs、CLI 出力、Evidence URI 等で扱う論理 hash は従来どおり `sha256:<64 lowercase hex>` のままであり、
object identity と hash 算出規約は変わらない。

**物理パスは digest-only 名の 1 表現のみである**。object、normalized instance/view、tombstone の
いずれも、読み書きともに上記の digest-only 名だけを解決する。同一 identity に対する第二の物理表現は
存在しないため、「複数表現が食い違ったらどうするか」という問いも生じない
(digest-only 名は `:` を含まないため、この 1 表現が全 OS で physical leaf になれる)。

tag の新規物理 leaf は上記の固定 ASCII hash 形式を使う。論理 tag 名は OS 非依存の portable
leaf 規則 (Windows 予約名、`<>:"/\\|?*`、control、末尾 dot/space を禁止) を満たす必要があり、
NFC 正規化 + Unicode **simple case folding (locale 非依存 — full folding・locale 別規則は使わない)** が同じ名前は case-insensitive collision として同一 slot を占める (folding は Unicode 安定性方針により割当済み文字で版間不変 — 版の記録は不要。**実装同梱の UCD 版で未割当の code point を含む tag 名は `KIO-E-CONFIG-USAGE-001` で拒否する** — 未割当→割当の版間遷移は folding を変え得るため、拒否により「割当済みのみ」の安定性前提を全域で成立させる)。正規化規則を変える場合は `kio_format_version` の非互換変更として扱う。未公開期間に旧規則の names.jsonl から ref を再導出する reader/migration は置かない。不一致は current store の corruption ではなく **incompatible format** として、store bytes を変更せず fail-closed で報告する ([10-operations.md §11.5](10-operations.md))。
`HEAD` の case variant は論理 tag 名として予約する。tag ref は `refs/tags-v1/tag-<digest64>` の
1 表現のみであり、第二の tag namespace は存在しない。論理 tag 名と tree entry は current schema の
portable-name 規則を満たす。対象 OS で物理化できない名前を例外的に読取専用で受理する旧 tree/path
分岐は持たない。

**論理名の truth**: digest は一方向であり、canonical ref (leaf + commit_hash 値) からは論理 tag 名を
復元できない。tag 作成時に `refs/tags-v1/names.jsonl` (append-only JSONL) へ
`{ digest64, logical_name (NFC 原表記), recorded_at }` を append する — これが論理 tag 名の truth
(書込は [04-pipeline.md §1.1](04-pipeline.md) の fsync 規律。**torn tail は chunks.jsonl と同型** —
末尾の不完全行のみ切り詰めて無視し ([05-runtime.md §8.1](05-runtime.md))、途中の malformed 行は
corruption)。**書込順序 =
names 行 append (fsync) → ref 作成** (逆順は crash で名前なし ref を作る)。列挙・表示は
names.jsonl で digest を解決する。対応行の無い canonical ref は fsck が corruption として報告する
(fsck は全行の schema と digest ↔ logical_name の対応、canonical ref ↔ 最終有効行の対応も検査する —
[10-operations.md §7.5.1](10-operations.md))。ref の無い names 行は tag 削除後の残存として正常
(順序の帰結でもある — 削除 `kio tag --delete` は `.kio/.lock` 下で ref のみを atomic に除去し
names 行は残す [06-cli-spec.md §1](06-cli-spec.md))。同一 digest の複数行は最終行を表示名とする
(NFC + simple case folding が同じ名前は同一 slot — 表記ゆれの上書きは append で表現)。

2026-10-03に、以前のUnicode小文字化を同梱UCD16.0.0によるsimple case foldingへ是正し、
同UCDの未割当tag文字を拒否する。sigmaの `Σ` / `σ` / `ς` は同じslotになり、`ß` と `ss` は
別slotである。Turkic foldingや複数文字へ展開するfull foldingは使わない。
保存先hashが変わる名前があるため、保存形式3.0.0で旧規則と区別する。
是正と最終候補の検証状態は [実行記録](../tasks/v1-closeout-execution-2026-10-03.md) を参照する。

**format_version**: 旧称 `VERSION 0.1.0` (旧 research/kio.md) は `kio_format_version` に統一。semver は [10-operations.md §11.5](10-operations.md) 参照。**保存場所 = `.kio/scope.json` の必須 `kio_format_version` フィールド** (init 時に current version を記録する)。現在の保存形式は `KIO_FORMAT_VERSION = "3.0.0"`。これは製品の release/Cargo version や製品v2/v3の到達状態とは独立した storage format である。3.0.0はtagのUnicode identity是正を含む。current reader は `KIO_FORMAT_VERSION` と**完全一致**する string だけを受理する。2.0.0を含む旧形式、欠落、非 string、非 parseable、older/newer、未知を含む任意の不一致は current schema ではなく **incompatible format** として、`KIO-E-STORE-VERSION-001` / exit 8 で store bytes を変更せず fail-closed にする。**この完全一致判定は scope.json の schema validation より先に評価する**。reader / search / repair / historical を含む全 command に read-only、migration、old-reader の例外はなく、multi-scope search も当該 scope を除外して partial success を返さず command 全体を停止する。具体挙動は [10-operations.md §11.5](10-operations.md) が正本。

**store gate（2.0.0で導入、3.0.0でも必須）**: `.kio/.store-gate` は init が全 OS で作成する必須の空 regular file であり、内容と identity を不変に保つ。削除・置換・遅延作成・repair による補完はしない。Windows の reader/writer coordination はこの恒久 gate を共有する。旧 Windows writer が gate を迂回して書き込むことを防ぐため、保存形式を 1.0.0 から 2.0.0 に変更した。旧 store は gate の有無によらず version 判定を先に行って exit 8 で拒否し、current store の gate 欠落・非 regular・非空も fail-closed とする。移行経路や旧形式の read/write 互換経路は置かない。

## 2.1 normalized instance と全文 view

**normalized の正本は manifest CAS が参照する NormalizedUnitObject CAS 群** ([04-pipeline.md §2](04-pipeline.md))。1 つの
`(raw_hash, tool_profile_hash, gen)` の組を **normalized instance** と呼び、
`objects/normalized_units/ab/cd/<raw64>.<tool64>.g<gen>/` ディレクトリ全体で表現する。

manifest schema:

```json
{
  "raw_hash": "sha256:abc...",
  "tool_profile_hash": "sha256:tool1...",
  "gen": 0,
  "parent_gen": null,
  "parent_instance": null,
  "run_id": "run_01H...",
  "units": [
    {
      "order": 0,
      "unit_key": "page:1",
      "unit_ref": "00f081779b832543",
      "unit_type": "page",
      "status": "done",
      "prepared_hash": "sha256:...",
      "preparation_profile_hash": "sha256:...",
      "unit_object_hash": "sha256:...",
      "error_kind": null
    },
    {
      "order": 56,
      "unit_key": "page:57",
      "unit_ref": "d2255263b6d52dc8",
      "unit_type": "page",
      "status": "failed",
      "prepared_hash": "sha256:...",
      "preparation_profile_hash": "sha256:...",
      "unit_object_hash": null,
      "error_kind": "invalid_input"
    }
  ],
  "generated_at": "2026-04-25T12:00:00Z"
}
```

`NormalizedUnitObject` schema (CAS body; `<unit_ref>.json` はその current projection):

```json
{
  "unit_key": "page:12",
  "unit_type": "page",
  "raw_hash": "sha256:abc...",
  "prepared_hash": "sha256:...",
  "preparation_profile_hash": "sha256:...",
  "tool_profile_hash": "sha256:tool1...",
  "gen": 0,
  "mode": "full",
  "markdown": "## 3.2 認証仕様\n...",
  "owned_image_hashes": [],
  "metadata": {
    "page": 12,
    "bbox_annotations": []
  },
  "reused_from": null,
  "generated_at": "2026-04-25T12:00:00Z"
}
```

`NormalizedUnitObject` は上記 unit object schema 全体を RFC 8785 JCS で保存した immutable CAS object である。
`unit_object_hash = sha256(JCS(full NormalizedUnitObject))` とし、保存先は
`objects/normalized_unit_objects/ab/cd/<unit-object64>` である。`reused_from` は unit_mapping
([04-pipeline.md §2.2](04-pipeline.md)) による再利用の provenance:
`{ "raw_hash": "sha256:old...", "gen": 0, "unit_key": "page:11" }`。再利用時も新 instance 用の
full NormalizedUnitObject を JCS CAS として確定する (per-.kio 重複容認、§9)。
`owned_image_hashes` は、この unit の処理で取得・検証した画像の content hash を保持する必須配列である。
重複のない昇順の SHA-256 hash を格納し、画像がなければ `[]` を明示する。通常の Markdown リンクは
画像の所有関係を作らない。OCR Adapter は受け取った画像バイトから計算した hash だけを返し、Kio は
単体画像の検証済み原本 hash を追加する。unchanged unit では従前の集合をそのまま引き継ぐ。
画像の読み出し・送信・検索 projection は、現在の管理境界で許可される所有元をこの immutable field から
検証する。本文や metadata に同じ hash を書いても所有関係は成立しない。field 欠落を空集合へ読み替える
旧形式互換や暗黙の移行は行わない。
`metadata` は current schema の **required object** であり、page/bbox/confidence と Step 4 の bounded
`bbox_annotations` を保持する。内容が無い場合は空 object `{}` を明示する。field 全体の欠落を `{}` に
読み替える旧 reader / default は置かず reject する。検索用 annotation block は同じ unit の `markdown` にも
決定論的に materialize されるため、chunk span と Evidence 解決元がずれない。

不変条件:

- **確定 manifest の `status=done` entry は non-null `unit_object_hash` を必須とし、その CAS object の full body と
  hash が一致しなければならない。`status=failed` entry は `unit_object_hash: null` を明示しなければならない**。
  field 欠落、done の null、failed の non-null は current schema violation として fail-closed にする。旧 shape を
  default / migration で読む経路は置かない。
- **各 manifest entry の `preparation_profile_hash` は必須の SHA-256 identity** である。prepared bytes の
  content address (`prepared_hash`) には混ぜず、unit 再利用と raw 不変時の gen 判定では両方を照合する。欠落・
  不正 hash は current-schema corruption として fail-closed にし、旧 shape を読む default / migration は置かない。
- path-named `<unit_ref>.json` は current loader がこの CAS object から再 materialize できる mutable working
  projection であり、履歴、`--at`、Evidence Pointer は決してこれや同 gen の「最新」body を読まない。
- manifest の `units[].error_kind` は [04-pipeline.md §5.3](04-pipeline.md) の閉 enum (フリーテキストではない) —
  unit 単位の retry 可否の機械判定に使う ([10-operations.md §11.1](10-operations.md) の明示例外)
- manifest の `units[].status` の遷移は `failed → done` の一方向のみ (部分失敗の再開、§6)。
  done unit の差し替えは新 gen 作成のみ (`kio reindex --regenerate`、または prepared_hash 変化起因の
  自動 gen+1 — 下記 gen 段落の例外)
- **manifest の各確定版は immutable object として保存する**: manifest の finalize (初回確定と、partial retry で
  `failed → done` を反映した各確定) のたびに、canonical JCS bytes を `objects/manifests/ab/cd/<manifest64>` へ
  content-addressed で書く (post-write verify 対象)。path-named `manifest.json` は**最新版の作業コピー**であり、
  過去版の解決は manifest object のみが担う。tree entry の `normalize.manifest_hash` (§8) は常に対応する
  manifest object を指すため、same-gen partial retry で作業コピーが更新された後も、過去 commit 時点の
  unit 完成状態と各 unit body を正確に列挙・検証できる。すなわち tree の `NormalizeRef.manifest_hash` は
  manifest CAS → `units[].unit_object_hash` CAS を transitively pin する (fsck の照合 =
  [10-operations.md §7.5.1](10-operations.md))

**gen (generation)**: 同一 `(raw_hash, tool_profile_hash)` に対する instance の世代番号 (0 起点の整数)。
通常は `g0` のみ存在する。`gen = 現最大 + 1` の新 instance を作れるのは `kio reindex --regenerate` と、
**prepare profile / renderer 変更による `prepared_hash` 変化が駆動する再 Markdownize** (§6 — first-instance-wins の
第二の合法経路。オンライン課金を伴うため 04 §4.6 と同型の確認プロンプト + budget guardrail の対象) だけであり、
既存 instance は保全する ([07-adapter-spec.md §9](07-adapter-spec.md))。identity はあくまで
`(raw_hash, tool_profile_hash)` であり、gen は同一 identity 配下の instance の区別にのみ使う。
**normalized_hash の代替ではない** (§5: Markdown の content hash は計算・保存・比較しない)。
新規参照 (新規 commit の tree entry / 新規 chunk) は常に最新 gen を使う。

**全文 view**: `objects/normalized/ab/cd/<raw64>.<tool64>.g<gen>.md` は
unit を決定論的に結合した **再生成可能な cache** であり、正本ではない。組み立て規則:

1. manifest.units を `order` 昇順に走査する (`order` は unit 間で一意 — 重複は KIO-E-STORE-CORRUPT-001 の corruption。値自体で順序が確定するため tie-break は存在しない)
2. `status = done` の unit は、その `markdown` から末尾の連続する改行を除去した文字列を採用する
3. `status = failed` の unit は、固定文字列 `<!-- KIO-MISSING-UNIT <unit_key> <error_kind> -->` を採用する (unit_key / error_kind は comment-safe に挿入する — `--` を含む値は percent-encode。生値の挿入は comment を途中終端し view の構造を壊す)
4. 採用した文字列を `"\n\n"` で結合し、末尾に `"\n"` を 1 つ付す — これが view 本文
5. §10 のヘッダコメントを本文の前に付す。chunk の byte offset は **unit-local** (当該 unit の
   `markdown` 本文 UTF-8 bytes 先頭を 0 とする byte span、§8.1) であり、全文 view 上の位置・ヘッダ・結合順は
   chunk identity に影響しない

view の破損・喪失・直接編集は `kio repair` による再生成で解消する。up_to_date 判定 (§6) に
view の存在は使わない。

# 3. スコープ境界 (重要)

各 `.kio` が管理するのは **その `.kio` が配置されたフォルダ直下のファイルのみ** である。この規則は次の 3 点で一意に定まる:

1. 管理対象は scope フォルダ **直下** のファイルに限る。サブフォルダ配下のファイルは、そのサブフォルダに `.kio` が存在するか否かに関わらず、親 `.kio` の管理対象に **ならない** (再帰包含は行わない)。
2. サブフォルダは常に独立スコープの候補である。v1 の管理 root に対する `kio index` は、下記の policy 境界内にある全子フォルダ（空フォルダと全ファイルが ignore されたフォルダを含む）を探索し、子 `.kio` の生成と各 scope の index を行う。watch も OS 通知と定期照合から同じ処理を使う。独立した子作成 debounce/SLA は定義せず、照合間隔は `watch run --reconcile-interval-seconds`（既定 300 秒）で指定する。macOS / Linux / Windows の実装と native Actions 受入の完了は区別する ([06-cli-spec.md §1](06-cli-spec.md), [09-mvp-scope.md §1.2](09-mvp-scope.md), [10-operations.md §4](10-operations.md))。ignore されたサブツリーには子 `.kio` を生成しない。親で有効な config ignore → `.kioignore` は子の直下スキャン、孫の探索、後日の child-only `index` / task 再検査にも同じ順序で適用される。実装は子 `.kio/config.toml` の strict な生成済み親ポリシーとして prefix 付きで保持し、子固有の config / `.kioignore` はその後に評価する。preview は親直下だけでなく各 planned child の直下件数・bytes と全 scope 合計を表示する。**VCS リポジトリ root (`.git` 等の VCS 管理ディレクトリを持つフォルダ) とその配下にも既定では子 `.kio` を生成しない** (skip + status 表示。`[scope] index_vcs_repos = true` で opt-in) — リポジトリの履歴は VCS 自身が持ち、`.kio` の自動生成はリポジトリを汚す ([01-positioning.md §8](01-positioning.md) の方針の機械化)。既存子 scope を grandfather する分岐は置かない。
   v1 の platform support level と子 scope 経路の対象範囲は [09-mvp-scope.md §1.2](09-mvp-scope.md) が正本である。macOS / Linux / Windows で retained parent handle から child を検証し、共通 app 層へ保持済み Repository を渡す自動管理経路を実装している。public pathname の再解決や `current_dir(path)` への handoff は使わない。独立 root、現行 policy の除外、identity 不一致、symlink / junction / reparse point は採用しない。実装済みという状態と、同一候補に対する native Actions 受入完了は別である。
3. したがって tree entry の `path`、Evidence Pointer の `path_at_commit`、task の `input_path` は **パス区切り (`/`) を含まないファイル名** である。`/` を含む path を持つ tree / pointer は schema violation (`KIO-E-STORE-PATH-001`) として拒否する。同様に `\` ・単独の `.` / `..`・NUL・control 文字を含む path、および **well-formed UTF-8 でない byte 列の path** も拒否する (tag の portable leaf 規則と同水準 — §2。JCS 直列化と UTF-8 バイト列昇順ソート (§8.1・§8) は well-formed UTF-8 を前提とするため、不正 byte 列は可逆に表現できない)。restore 等の物理化は canonical join 後に対象ディレクトリ配下であることを検査する ([06-cli-spec.md §5](06-cli-spec.md))。この検証は read/write を問わず current canonical object に適用する。区切りを含む旧 tree/path や raw-name pointer を読取専用で受理・安全名 mapping・legacy warning に落とす分岐は持たない。

ファイルの位置は `scope_path` (正本 `.kio` の絶対パス) + ファイル名で一意に表現される。「フォルダ木を横断してファイルを探す」体験は、個々の `.kio` の再帰包含ではなく、scope_registry が列挙し aggregator が採点する横断検索 ([05-runtime.md §1.8](05-runtime.md)) が担う。

```
親 .kio と子 .kio 間で同一ファイルが二重 object 保存されることは発生しない。
別 .kio 間の同一内容ファイルは、ユーザーが意図的に複数フォルダへ配置した場合に限り
物理的重複保存を許容する (per-.kio dedup, cross-.kio dedup なし)。
```

# 4. 二層構造 — truth vs cache

```
truth = folder-local .kio           raw object / normalized / chunks / commits / refs
cache = scope_registry             検索の探索対象一覧 / stale 検出
        aggregator                 全 scope の index を複製した device-level read replica
```

この二層図は**知識・scope・送信承認**についての分類である。課金台帳、provider へ送った
in-flight intent、回復・照合に必要な運用記録は knowledge ではなく、device/central に置ける
非再構築可能な運用上の truth である。`.kio` の復元から reset してはならず、`.kio` の backup とは
別に backup と reconcile を行う。詳細な現行手順は [10-operations.md §7.5.2](10-operations.md)。

`scope_registry` 保存先: `~/.local/share/kio/scope-registry.sqlite`。**device data dir の実体は
`${XDG_DATA_HOME:-$HOME/.local/share}/kio` であり、本仕様の `~/.local/share/kio/` 表記は全て
この解決結果を指す表記規約とする** (backup 例 [10-operations.md §7.5.2](10-operations.md) と
実体が分裂しない — runtime と backup が同じ path 解決を共有する)。

`aggregator` 保存先: `${XDG_CACHE_HOME:-$HOME/.cache}/kio/aggregator.sqlite` (**cache root** —
全内容が各 `.kio` から再構築可能なので data root には置かない)。横断検索は 2026-07-25 に
scatter-gather から **replication** へ変更した ([05-runtime.md §1.8](05-runtime.md))。428 scope の
device では `.kio` ごとに独立した BM25 コーパスができ、コーパス統計 (N / df / avgdl) が index ごとに
異なるため text 順位が scope 間で比較不能になる — 実測で正解が自フォルダ内 1 位でありながら横断 38 位に
沈んだ。**コーパスを 1 つにすればこの問題は定義上消える**ため、採点と候補選択を単一 replica の上で行う。

不変条件:

```
1. scope_registry / aggregator のみで .kio の状態を変える実装は禁止。
   **反映順序は必ず「各 scope の索引 → aggregator」とする (2026-08-11)** —
   逆順は「検索に出るのに開けない結果」を作る。検索は aggregator しか
   読まないので、replica の先行はそのまま利用者に見える (05 §1.8
   「書き込み順序」)
2. scope_registry / aggregator 喪失は再構築可能 (各 .kio を rescan)。
   **「再構築可能」であって「欠損中も動作」ではない** (2026-08-11 明確化) —
   aggregator を失った検索の正しい挙動は、検索を fail-closed とし、次の
   `kio index` / `kio reindex` / `kio repair` が全射影を完了するまで待つこと
   であり、第 2 の検索実装や読取り時の lazy refresh を持つことではない。
   この読み違えが scatter-gather 経路を存置させていた
3. .kio 喪失は復旧不能 (検証とバックアップの運用は 10-operations.md §7.5)
4. 検索結果メタには「正本の .kio パス」を必ず含める
5. raw object の所有権・dedup は scope_registry でグローバル化しない
6. aggregator は安全性判定の最終権限を持たない — purge journal は、
   結果を返す scope について live .kio で再確認する (05 §1.8 の手順 3)。
   検索は source SQLite を開かない。replica header の `Rebuilding` と projection
   完全性は入口で確認し、返却直前に source index から取り直さない。候補に現れた
   scope の purge journal / epoch / lifecycle barrier だけは応答境界の直前に再確認する。
   履歴 selector
   (`--at` / `--all-history` / `--since` / `--include-deleted`) と cursor replay の
   source CAS preflight は、CAS から解決し直した exact binding relation を runtime
   eligibility filter として replica query に渡す。この filter は `agg_bindings` との
   一致だけを許し、FTS / vector / image の `candidate_depth` 適用より前に効く。
   source の candidate SQL、materialize、射影修復への fallback ではない
7. aggregator は候補の「選択と採点」を担い、liveness 判定を再実装しない。
   writer / repair の完全射影時に scope resolver が解決した selector / snapshot ごとの結果を
   `agg_bindings` として持つ。binding は chunk identity と `path_at_commit` /
   `pointer_commit` を結ぶ relation であり、config 切替、DAG の分岐・再導入、
   同一 chunk の複数 alias を scope 側の答えどおりに表す。replica は scalar provenance
   metadata から eligibility を推定しない。検索時は binding から作る eligible 集合を
   `WHERE` で参照する。fresh current 検索は durable binding を使う一方、履歴 selector
   と cursor replay は CAS で再解決した exact relation を `agg_bindings` に交差させる。
   この pre-rank filter も replica 内で候補を絞るためのものであり、source SQLite を
   検索面へ戻すものではない。
   全 committed chunk は `agg_chunks` と単一の FTS / vector collection に一度だけ置く。
   分表にすると live と過去が別コーパスになり、--include-deleted が比較不能な
   2 群を 1 つの順位に混ぜることになる。**検索が読む索引は aggregator ただ 1 つとする** —
   scope 数によらず `.kio/index/sqlite.db` は検索面ではない。同じ問いに答える
   派生索引を 2 つ持てば、答えが経路によって変わる (05 §1.8「検索は
   `.kio/index/sqlite.db` を引かない」)
8. 権限の書き込みは常に .kio へ行う。aggregator は投影のみで、
   送信 gate の可否を aggregator の行で判定してはならない
```

不変条件 6-8 の理由。**6**: replica の staleness をそのまま信じると「purge 中の scope を読む」に化ける。
かといって毎回全 scope を開けば replication の意味が無い。結果を出した scope だけ検証すれば
コストは `O(結果ページの distinct scope 数)` で scope 総数に依存しない。
**初版は「消えたはずの chunk を返す」も理由に挙げ、`kio_format_version` と `index_generation` の
返却直前再確認まで求めていた。2026-08-11 に取り下げた** — この 2 値は入口ガードで足りる。
ただし候補 scope の purge journal / epoch / lifecycle barrier は別であり、応答境界直前の `recheck()` を
維持する。前者を再確認して守れるのは検索 1 回分の窓にすぎず、その窓で古い行が残っても、pointer は
参照解決時に完全な安全確認を通り、結果行は本文を持たない ([05-runtime.md §1.8](05-runtime.md) 手順 3)。
purge を残すのは法務・秘匿の操作であり**文書名だけでも意味を持つ**ためである。
**7**: `chunk_config_generations` / `tree_entries` / publication / ancestry を aggregator 側で
組み直すと liveness 判定が 2 箇所になり、必ず乖離する。生テーブルではなく **scope 側の既存 resolver が
解決した binding**を複製し、candidate query はその binding を `WHERE` で参照する。**8**: aggregator が
古くても「未承認なのに送信される」が起きてはならない。

## 4.1 永続ストア一覧 (technology × truth/cache)

各永続ストアの実装技術と喪失時の扱いの正本表 (2026-07-14 実装準拠で確定)。個別 schema の正本は右端の参照先に置く。

| ストア | 技術 | 区分 | 喪失時 | schema 正本 |
|---|---|---|---|---|
| `.kio/objects/` (raw / prepared / image / normalized_unit_objects / manifests / toollocks / chunks / embeddings / trees / commits) | file (CAS。`normalized_units/` はこの inventory 外の path-named current projection / cache であり、`manifest.json` / `<unit_ref>.json` は CAS closure から再 materialize 可。immutable truth は manifests + normalized_unit_objects — §2.1) | **truth** | 復旧不能 (検証: [10-operations.md §7.5](10-operations.md)) | §8 / §2.1 |
| `.kio/HEAD` / `refs/` | file (atomic rename) | **truth** | 復旧不能 | §2 |
| `.kio/tombstones/` + `.kio/purge/erase-receipts/` (erase receipt) | file | **truth** (purge 証跡) | 復旧不能 | [05-runtime.md §3.5](05-runtime.md) |
| `.kio/purge/journal` | file (単一 JSON、temp + rename — [04-pipeline.md §1.1](04-pipeline.md)) | **truth** (active purge の crash 回復正本 — 対象 closure と phase を耐久記録し各 phase を冪等再開する。完遂で削除 = 定常時は不在) | active purge 中の喪失は phase 再開情報の喪失 (closure の残骸・不整合は `kio repair verify-objects` が corruption として検出 — [10-operations.md §7.5.1](10-operations.md)) | [05-runtime.md §3.5](05-runtime.md) |
| `.kio/scope.json` / `config.toml` / `tool-lock.json` | JSON / TOML (schema 検証: [10-operations.md §11.3](10-operations.md)) | **truth** | 復旧不能 | 各 spec |
| `.kio/logs/access.jsonl` | JSONL (append-only) | **truth** (access_events の正本) | 復旧不能 | §2 |
| `.kio/purge/epoch` | 単調カウンタ (text) | **truth** (purge の ABA barrier) | 欠落 = 読取 fail-closed。次の locked mutation が journal の target_epoch、journal も無ければ全 lifecycle event の `epoch` 最大値 + 1 (event 皆無なら 1) から回復して再作成 ([05-runtime.md §3.5](05-runtime.md)) | [05-runtime.md §3.5](05-runtime.md) |
| `.kio/tombstones/lifecycle-epoch` | 単調カウンタ (text) | **truth** (lifecycle 回転補完の検出源) | 欠落・不正・巻き戻りは max(last_lifecycle_epoch, 全 lifecycle event の lifecycle_epoch 最大値) + 1 で再作成 + 無条件 1 回転 (purge の `epoch` は参照しない — 別系統のカウンタ) ([05-runtime.md §3.5](05-runtime.md)) | [05-runtime.md §3.5](05-runtime.md) |
| `.kio/manifest.json` | JSON (schema 検証) | working-state cache (永続的真実は tree/commit object) | rescan で再構築 | §8 files |
| `.kio/tasks.jsonl` | JSONL (append-only) | 運用データ | 喪失許容 ([04-pipeline.md §5.7](04-pipeline.md)) | [04-pipeline.md §5.1](04-pipeline.md) |
| `.kio/chunks.jsonl` | JSONL (append-only) | **truth** (chunk の世代 association / created_at / 生成時点 path / tagged publication events — chunk object には含めない §8) | 復旧不能 (SQLite rebuild の入力) | §8 / [04-pipeline.md §4.1](04-pipeline.md) |
| `.kio/index/sqlite.db` (**検索面ではないが aggregator の複製元である** — 2026-08-12。`kio search` は引かず、読むのは writer / repair による完全射影と scope 内保守。`.kio` 内にあることでフォルダごと移動しても移動先で再導出でなく射影で済む。[05-runtime.md §1.8](05-runtime.md)) | **SQLite** (public logical object は chunks / chunk_config_generations / chunk_publications / embeddings / tree_entries / index_metadata の 6 表、および chunk_fts / chunk_vec / image_vec の 3 virtual table、計 9。FTS5 / sqlite-vec の shadow table は含めない) | cache | 単一 scope は `kio repair rebuild-db`、device 全体は `kio repair all` (`-a`) | [04-pipeline.md §4](04-pipeline.md) |
| `~/.local/share/kio/scope-registry.sqlite` | **SQLite** (`scopes` 1 表) | cache | 各 `.kio` の rescan | [10-operations.md §3](10-operations.md) |
| `~/.cache/kio/aggregator.sqlite` | **SQLite** (`agg_scopes` / `agg_chunks` / `agg_fts` / `agg_embeddings` / `agg_image_embeddings` / `agg_image_refs` / `agg_bindings` / `agg_projection_markers` の 8 表) | cache | writer の完全再射影、または device-global の `kio repair replica` (`-r`)。source SQLite も含めて検証・再構築する場合は `kio repair all` (`-a`)。欠落・不完全時は `kio search` が source index を読まず fail-closed | [05-runtime.md §1.8](05-runtime.md) |
| `${XDG_CACHE_HOME:-$HOME/.cache}/kio/search-query-cache/<query_vector_digest>` | file (canonical float32 little-endian vector) | device-local cursor replay cache（query 本文は保存しない） | 欠落・digest 不一致は当該 cursor を `KIO-E-SEARCH-CURSOR-001` で拒否。再 embedding や source SQLite 読みには戻らない | [05-runtime.md §1.5](05-runtime.md) |
| `~/.local/share/kio/cost-ledger.sqlite` | **SQLite** (`cost_ledger` / `batch_requests` / `schema_migrations` の 3 表、WAL) | **device/central operational truth** (課金台帳 + **in-flight intent (Batch job / sync request) の正本** — [04-pipeline.md §5.8](04-pipeline.md)。knowledge ではなく、tasks.jsonl と異なり喪失許容ではない) | 確定課金は再構築不可 (Adapter 報告値の記録であり再導出元がない)。in-flight は batch 行が provider job 一覧の intent_token 全走査、sync 行が provider request id 照会 (照会不能は estimated 確定 — [04-pipeline.md §5.4](04-pipeline.md)) で回収 | [04-pipeline.md §5.4](04-pipeline.md) (SQL 正本) |

**SQLite を使うのはこの表の 4 ファイル (public logical object は計 21 表 / virtual table)**。内訳は index/sqlite.db 9、scope-registry.sqlite 1、aggregator.sqlite 8、cost-ledger.sqlite 3 である。FTS5 / sqlite-vec の shadow table は SQLite の内部実装であり、この logical-object 数にも public schema 契約にも含めない。うち index/sqlite.db・scope-registry.sqlite・aggregator.sqlite は正本から再構築可能な検索キャッシュ、**cost-ledger.sqlite だけは再構築不可の運用台帳** (cache ではない — current schema と不一致な既存 bytes は fail-closed、[10-operations.md §7.5.3](10-operations.md))。cursor replay の query vector は source の `embeddings` 表ではなく上記の device-local file cache に置くため、破棄・欠落の影響は cursor 拒否だけである。**SQLite ファイルとして cache root (`$XDG_CACHE_HOME`) に置かれるのは aggregator.sqlite だけ**だが、query-vector replay cache も同じ cache root の file である — 他の 3 SQLite は data root。区別の基準は「ユーザーが知る情報を失うか」であり、aggregator は各 `.kio` の射影に過ぎず何も失わない (§4 不変条件 2)。コンテンツの truth は引き続きファイル (CAS objects/ ほか) が正本であり、tasks.jsonl は喪失許容の JSONL のまま。`schema_migrations` は current operational marker 専用であり、旧 JSONL cutover の importer marker は置かない。課金 + in-flight intent の current SQLite record は UNIQUE・単一 Tx・ON CONFLICT 冪等の保証を正本要件とする ([04-pipeline.md §5.4](04-pipeline.md))。

# 5. Identity — hash と semantic_fingerprint の分離

```
raw_hash             原文バイト列の同一性 (1 バイト違えば別 object)
tool_profile_hash    Adapter capability の identity (§5.1)
tool_lock_hash       tool-lock の canonical 構成 (各 role の tool_id / profile_hash、embedding はさらに dimensions / distance / modality — §5.2 の入力) を畳み込んだ識別子
semantic_fingerprint 意味的・視覚的・構造的な近さ (page fingerprint, embedding 等)
```

ルール:

- 同一性判定 (up_to_date / dedup) には hash を使う
- 類似性判定 (重複候補提示, page reuse, 分類) には semantic_fingerprint を使う
- 命名で区別 (`*_hash` vs `*_fingerprint`)
- **Markdown content hash (normalized_hash 等) は採用しない**。Markdown は LLM ベース非決定的なため。Markdown 識別は `(raw_hash, tool_profile_hash)` のみ

## 5.1 tool_profile_hash 計算規約

artifact identity は `(raw_hash, tool_profile_hash)` 単独に依存するため、計算規約をプロダクト契約として固定する。

**ハッシュ対象フィールド (capability hash)** — 決定性に影響する情報のみ。`cmd`/`args`/`url`/認証情報は **絶対に含めない**:

```
adapter_kind          "prepare" | "markdownize" | "embedding" | ...
                      (OCR は adapter_kind ではなく capability — 07-adapter-spec.md §1)
adapter_role          "text" | "image" | "multimodal"
model_or_tool_family  "gemini-2.5-pro" | "gpt-4o" | "tesseract" の正規化名
model_version_pin     ベンダー側 immutable tag (latest 等の可変 alias は禁止)
                      **runtime_kind="local" かつ execution_mode="offline_api" (重みを持つ
                      ローカルモデル) では、重みファイル (GGUF / safetensors) の sha256 を
                      pin とする** — `gemma-3-4b-it-q4_k_m` 等のタグ名は量子化違いで同名に
                      なり得て、ベンダー側 immutable tag と同じ強さを持たないため。
                      **重みが複数ファイルに shard されている場合は、モデルディレクトリからの
                      相対パスをキーとした sha256(JCS({relative_path: sha256, ...})) を pin と
                      する。単一ファイルのモデルではそのファイルの sha256 をそのまま採る**
                      (集約式を通さない — pin が配布元の blob hash と一致し、ダウンロード
                      健全性の確認をそのまま兼ねるため)。対象は拡張子 `.safetensors` /
                      `.gguf` / `.bin` / `.pt` のファイルで、相対パスにするのは pin を保管
                      場所から独立させるため。
                      **deterministic_library の同梱 Adapter は semver 規約のまま**
                      ([07-adapter-spec.md §2.1](07-adapter-spec.md) の PDF text layer 抽出が
                      1.0.0 → 1.1.0 として運用中 — 重みを持たないため sha256 が定義できない)
prompt_template_id    Kio が管理する prompt 識別子
prompt_template_hash  prompt 本文を canonical 化した sha256
sampling              {temperature, top_p, top_k, max_tokens, seed}
output_schema         期待する Markdown / JSON schema id とバージョン
render_params         prepare 専用: {renderer_name, renderer_version, dpi, color_space, output_format}
                      (バイト列決定性に影響する全レンダリング設定 — [04-pipeline.md §2.1](04-pipeline.md))
bbox_annotation       markdownize 専用: boolean — folder config `[markdownize].bbox_annotation` の
                      実効値を採用時に畳み込む (値は出力に影響する — [07-adapter-spec.md §5.2](07-adapter-spec.md)、
                      schema key は [10-operations.md §11.3](10-operations.md))
dimensions / distance / modality   embedding 専用
runtime_kind          "cloud" | "local" (capability レベル)
spec_version          この計算規約自体のバージョン
```

実装バイナリのバージョン (`adapter_binary_version`, OS, ハードウェア) は **`binary_hash` として別保存**し、`tool_profile_hash` には含めない。これにより Adapter のマイナー bug fix で全 re-index が走らない。

**算出式** (RFC 8785 JCS 準拠):

```
tool_profile_hash = "sha256:" + base16(sha256(JCS(canonicalize(profile_fields))))
```

null フィールドは hash 入力に含めない (省略と null を識別しない)。

**prompt_template_hash**:

```
1. trim trailing whitespace per line
   (行末の ASCII 空白 U+0020 と TAB U+0009 のみ除去。全角空白・\f・\v は除去しない。
    行区切りは CRLF / LF / 単独 CR のいずれも行区切りとして扱う)
2. normalize line endings to \n (CRLF → \n、単独 CR → \n)
3. NFC 正規化
4. 末尾の空行を削除
5. UTF-8 バイト列に対し sha256, "sha256:" プレフィックス
```

手順 1-2 の対象文字集合と単独 CR の扱いは 2026-07-03 に確定 (契約テスト設計 step2a §C-4 の決定性論点解消)。

`spec_version` の bump は breaking change 扱い。current reader は選択された current version だけを受理し、migration / old-version read-only reader は置かない。

## 5.2 tool_lock_hash 計算規約

commit object 等で参照される `tool_lock_hash` は tool-lock の canonical 構成の identity (下記入力のみ — 作業コピー `tool-lock.json` 全体ではない。[07-adapter-spec.md §6](07-adapter-spec.md)):

```
tool_lock_hash = "sha256:" + base16(sha256(JCS({
  spec_version: <int — 現在値 = 1>,
  prepare:        { tool_id, profile_hash },
  markdown:       { tool_id, profile_hash },
  embedding:      { tool_id, profile_hash, dimensions, distance, modality }
})))
```

**preimage の保存**: tool-lock の materialize ([07-adapter-spec.md §6](07-adapter-spec.md)) 時に、この
canonical JCS bytes を `objects/toollocks/ab/cd/<hash64>` へ content-addressed で保存する (immutable) —
commit の `tool_lock_hash` から当時の **lock 構成 (各 role の tool_id / profile_hash の組)** を
復元・検証できる (manifest object と同族。fsck の再 hash 対象 — [10-operations.md §7.5.1](10-operations.md))。
**profile_hash の preimage (model pin・Adapter 定義本体) はデバイスローカル tools.toml 側にあり、
hash からの逆算・内容復元は保証しない** — toollock object の目的は identity の検証であって
過去 profile 内容の再現ではない (§11)。
作業コピー `tool-lock.json` は最新版であり、過去版の解決は toollock object のみが担う。

`cmd`/`args`/`url`/`config_hash`/capabilities は入力に含めない。embedding のみ次元・距離・modality を含めるのは、横断検索互換性 (§7) の決定根拠になるため。optional adapter は未設定なら省略 (null と識別しない)。

## 5.3 chunking_config_hash 計算規約

chunk 境界は Adapter ではなく core 側の chunking 設定 (`.kio/config.toml [chunking]`、§11) で決まるため、`tool_profile_hash` には畳み込まれない。chunk / embedding の世代判定用に独立の hash を持つ:

```text
chunking_config_hash = "sha256:" + base16(sha256(JCS({
  spec_version: <int — 現在値 = 1>,
  strategy: <effective [chunking].strategy — 既定 "heading">,
  max_chars: <effective [chunking].max_chars — 既定 6000>,
  unicode_version: <slug 正規化に用いる Unicode (UCD) 版 — 04 §4.1 の固定文字集合と連動。
                    版差は config 変更として現れる。**省略不可 (default なし)**。
                    property は UCD の Script (Script_Extensions は使わない)>
})))
```

- 対象は `[chunking]` 配下の **chunk 境界に影響する全キー**。キーを追加したら `spec_version` を bump する。**キー不変のまま境界を変える分割意味論の改訂 ([04-pipeline.md §4.1](04-pipeline.md) の決定規則の変更) も bump の対象** — hash 変化が §4.6 の再 chunk を発火する (2026-07 の決定規則明文化は実装・store 公開前の定義確定であり bump しない — 08 の schema_version 規約と同型)
- デフォルト値も明示的に畳み込む (キー省略と明示指定を識別しない)
- `unicode_version` は **`kio init` が採用 UCD 版 (現在の既定 = 17.0.0 — §11 の設定例と同一。実装が同梱する UCD 版と常に一致させる) を config へ明示記録する** ([06-cli-spec.md §1](06-cli-spec.md) の init 仕様・[10-operations.md §11.3](10-operations.md) の schema required も同旨)。これを欠く config は schema error / fail-closed とし、同梱版で読み替えたり次回書込みで補完したりしない
  (「省略不可・default なし」の充足手段 — 以後の版変更は config 変更として本節の世代判定に乗る)
- これは同一性 hash であり、identity には使わない。chunk identity は §8.1 のとおり `(raw_hash, tool_profile_hash, gen, unit_key, unit_content_hash, heading_path, section_id, byte_start, byte_end)` とする。`unit_content_hash` は exact Markdown bytes の hash なので同一 gen の本文変更を分離しつつ、同一本文の再取り込みでは identity を維持する。`chunking_config_hash` は chunk の**世代**を表すメタデータに留める

# 6. Up_to_date 判定

ファイルが Markdown 化済みかの判定は、最新 normalized instance の manifest と unit object の存在 (§2.1) のみで決定する。Markdown content hash 一致は **判定条件に含めない** (§5)。

```python
current_raw_hash = hash(file)
inst = latest_instance(current_raw_hash, current_tool_profile_hash)
       # objects/normalized_units/ 配下の最大 gen の manifest (§2.1)
if inst is None:
    pending
elif not inst.units:
    up_to_date       # 空 unit 集合 (空文書) — 次行の all([]) が空虚真で failed に落ちるのを防ぐ
elif all(u.status == "failed" for u in inst.units):
    if all(u.error_kind is permanent for u in inst.units):
        settled      # 全滅かつ全て permanent — terminal。再投入対象なし
                     # (04-pipeline.md §5.2 の failed permanent / settled partial と同族の terminal —
                     #  cleanup・blocker 除外の扱いは同じ。脱出は新 gen のみ)
    else:
        failed       # 全滅 (retryable を含む) — retryable (04-pipeline.md §5.2 の failed → pending と同じ扱い)
elif any(u.status == "done" and not unit_object_exists(u) for u in inst.units):
    missing_output   # done 宣言 unit の object 欠落 — failed unit と併存しても partial で隠さない
                     # (回復対象 = 欠落 done ∪ retryable failed の和集合 — permanent failed は
                     #  04-pipeline.md §5.2 の settled 扱いのまま再投入しない。欠落を先に判定しないと
                     #  permanent failed の陰で欠落が恒久に再投入されない)
elif any(u.status == "failed" for u in inst.units):
    partial          # 成功 unit は検索対象。再投入は retryable な失敗 unit のみ (permanent は除く —
                     # done + 残り全 permanent は 04-pipeline.md §5.2 の settled partial として terminal)
else:
    up_to_date
# prepare profile の変更は 04 §2.1 の prepared_hash 変化として再投入を駆動する — 本判定は instance の
# 存在のみを見るが、§2.1 の差分判定が上流で新 run を積むため up_to_date に留まらない
```

判定の正本は manifest + unit object の存在である (`normalization_runs` は SQLite テーブルとしては
持たない — §8。run 状態は manifest から導出し、復元セマンティクスは [04-pipeline.md §5.7](04-pipeline.md))。
全文 view の存在は判定に使わない。

ファイル状態分類:

```
new            初めて見つかった原文
up_to_date     最新 Markdown あり (unit 段の判定 — chunk 生成の完了は含まない。chunk が期待集合
               (当該 (raw, profile, gen, config) の決定論的 re-chunk 結果) に達していない間は
               index_status (05-runtime.md §1.7) が partial として可視化する)
modified       path 同じだが raw_hash が変わった
tool_changed   raw_hash 同じだが tool_profile_hash が変わった
partial        一部 unit の Markdownize が失敗 (成功 unit は検索対象、欠損は kio status に表示)
missing_output manifest は done を記録しているが unit object ファイルが見当たらない
failed         前回 Markdown 化失敗
settled        全 unit が失敗かつ全て permanent — terminal (再投入対象なし。脱出は新 gen のみ —
               04-pipeline.md §5.2 の settled partial と同族)
pending        実行待ち
```

`corrupted` (Markdown content hash 不一致) は採用しない。Markdown は read-only artifact として content hash を持たないため。

# 7. Embedding 互換性ルール

複数 `.kio` 横断 vector 検索の条件:

```
dimensions / distance / modality / embedding profile_hash がすべて一致
```

不一致なら BM25 のみ横断検索、または再 index 要求。

**modality は `"multimodal"` に固定する (2026-07-03 確定)**。text と image 等を別ベクトル空間に埋め込む
構成 (`modality="text"` 等の非 multimodal profile) は**採用不可**であり、tool-lock への materialize /
adapter 登録の時点で `KIO-E-EMBED-MODALITY-001` (exit 2、[06-cli-spec.md §8](06-cli-spec.md)) として
拒否する。採用 profile は [07-adapter-spec.md §5.3](07-adapter-spec.md) の単一マルチモーダル profile のみ。

# 8. 主要テーブル / object スキーマ

## files (working state) — 実体は manifest.json (SQLite テーブル非採用)

working state の実体は `.kio/manifest.json` の `files` 配列であり、**SQLite に `files` テーブルは
作らない** (§4.1。2026-07-14 実装準拠へ更新 — 旧 `CREATE TABLE files` 定義は未実装のまま廃止)。
schema は `kio-core/schemas/manifest.schema.json` (JSON Schema、[10-operations.md §11.3](10-operations.md))
で検証する:

```json
{
  "schema_version": 1,
  "updated_at": "2026-07-14T00:00:00Z",
  "files": [
    { "path": "report.pdf", "raw_hash": "sha256:ab...", "status": "modified" }
  ]
}
```

- `path` は自フォルダ直下のファイル名のみ (`/` を含まない — §3 スコープ境界)
- `status` は `new | modified | deleted | unchanged` の固定 enum
- ファイル削除を検出しても entry は **削除しない**。`status = "deleted"` に更新し、最後に観測した
  raw_hash を保持する。同一 path が再作成されたら status を戻す
- manifest は working-state cache であり (§2 レイアウト注記のとおり永続的真実は tree/commit object)、
  cursor-stable `--include-deleted` の truth は page-1 snapshot の first-parent trees から導出する
  ([05-runtime.md §1.6](05-runtime.md)); manifest/files の後発変更は paging 集合を変えない

旧テーブル定義にあった `file_id / size_bytes / mtime / kind / first_seen_at / last_seen_at` は
持たない。必要になった場合は manifest.json の `schema_version` bump で追加する。

## normalization_runs — 実体は normalized instance manifest (SQLite テーブル非採用)

normalization run の正本は normalized instance の `manifest.json` (§2.1) であり、**SQLite に
`normalization_runs` テーブルは作らない** (§4.1。2026-07-14 実装準拠へ更新)。run 状態は独立永続化
せず、manifest + unit object の存在から導出する (§6、[04-pipeline.md §5.7](04-pipeline.md))。

run レコード (パイプライン内部表現。manifest へは provenance として `run_id` / `parent_gen` を記録):

```text
run_id                                 manifest.run_id として永続化 (provenance)
raw_hash / tool_profile_hash / gen     instance identity (= instance ディレクトリ名 §2.1)
mode                                   full | incremental
status                                 pending | running | done | partial | failed
                                       (manifest の unit status から導出)
changed_unit_keys                      incremental の対象 unit ([04-pipeline.md §5.1](04-pipeline.md) task にも記録)
output_ref                             `normalized:<raw64>.<tool64>.g<gen>` (portable taskref。下記)
fallback_reason                        capability_missing | threshold_exceeded | ...
                                       (task 側の運用データ、喪失許容)
created_at / finished_at
```

`normalized_path` は持たない。Markdownize task の `output_ref` は `normalized:<raw64>.<tool64>.g<gen>` だけであり、`<raw64>` は `input_hash`、`<tool64>` は `tool_profile_hash` の各 digest-only lowercase hex、`<gen>` は leading zero を許さない canonical decimal `u64` である。parser は input hash と raw digest の一致、両 digest、generation、完全な 1 表記を mutation 前に検証する。absolute / relative pathname、separator、末尾 slash を含む旧 pathref は互換読取りせず拒否する。runtime の instance directory はこの taskref から retained current `.kio/objects/normalized_units/` の下へ導出する。

instance は `(raw_hash, tool_profile_hash, gen)` から一意に決まる。
世代の親子関係は manifest の `parent_gen` で表現する (`parent_run_id` チェーンは永続化しない —
喪失許容の運用データ、[04-pipeline.md §5.7](04-pipeline.md))。**incremental で親の raw が異なる場合
(raw 更新をまたぐ通常 incremental) は `parent_instance = {raw_hash, tool_profile_hash, gen}` を必須で
記録する** — `parent_gen` は同一 raw 内の局所番号であり、整数だけでは親 instance を一意に復元できない
(full では null)。**manifest_hash** (tree の入力 — §8) は manifest の canonical JCS bytes の
sha256 とする。

## 8.1 Object hash 算出規約

object hash は artifact identity と Evidence Pointer の永続性 (08 §6) を支えるプロダクト契約であり、`tool_profile_hash` (§5.1) と同じ厳密さで固定する。

**共通規則**:

- 論理 hash 表記は `"sha256:" + base16(sha256(...))` (小文字 hex)。JSON、SQLite、refs、CLI、URI ではこの完全表記を使う。
- **文字列を preimage とする hash (unit_ref の unit_key・tag の正規化済み論理名・prompt_template_hash の正規化結果等、JCS 系以外の全 textual preimage) は、規定の正規化を適用した後の UTF-8 バイト列に対して sha256 を計算する** (JCS 系は RFC 8785 が UTF-8 を内包)。encoding 無指定の実装差 (UTF-16 等) は store の可搬性と検証可能性を壊すため許容しない。
- canonical fan-out パスは `objects/<type>/ab/cd/<digest64>`。`digest64` は論理 hash から `sha256:` を除いた 64 文字の小文字 hex で、`ab` / `cd` はその先頭 2 文字 / 続く 2 文字。normalized basename と tombstone leaf も §2 の digest-only 規則に従う。
- object 本体は**自身の hash を含めない** (Git 同様、保存キーが ID。旧 `tree_id` / `commit_id` /
  chunk object 内の `chunk_hash` フィールドは廃止)。raw / prepared / image は保存 byte の content key、
  tree / commit は canonical JSON の content key、chunk は保存 path の key と §8.1 identity tuple を照合する。
- 人間向け表示は先頭 12 hex への短縮可 (`sha256:9f2c1a7b04de…`)。`--json` は完全 hash ([06-cli-spec.md §4](06-cli-spec.md))。
- hash 算出または論理 identity 規約の変更は `kio_format_version` の MAJOR bump。§2 の digest-only 物理名は、論理 hash を変えず Windows を含む filesystem で同じ object を表現する portability correction であり、identity の変更ではない。旧物理名を読む fallback は置かず、canonical digest-only physical name だけを解決する。

**raw / prepared / image** — content hash:

```text
raw_hash      = "sha256:" + base16(sha256(原本ファイルのバイト列))
prepared_hash = "sha256:" + base16(sha256(prepared object のバイト列))
image_hash    = "sha256:" + base16(sha256(抽出画像のバイト列))
```

**tree / commit** — canonical JSON の content hash:

- object は RFC 8785 JCS canonical form の JSON バイト列として保存する。
- `tree_hash` / `commit_hash` = 保存バイト列の sha256。検証は再ハッシュのみで足りる。
- 種別誤認防止のため object 本体に `"object_type": "tree" | "commit"` を必須で含める (Git の object header 相当)。
- tree の `entries` は `path` の UTF-8 バイト列昇順で一意にソートする。同一 `path` の重複 entry は禁止。
- commit の `parents` は commit_hash の配列。第一要素は直前 HEAD (first parent)。
- timestamp は UTC ISO8601 + `Z` ([06-cli-spec.md §11](06-cli-spec.md))。
- `HEAD` / `refs/heads/*` / `refs/tags-v1/tag-*` の値は commit_hash (`refs/tags-v1/names.jsonl` は ref ではなく論理名 ledger — この規則の対象外)。

**manifest** — canonical JSON の content hash (§2.1):

```text
manifest_hash = "sha256:" + base16(sha256(manifest の canonical JCS バイト列))
```

- 保存パスは `objects/manifests/ab/cd/<manifest64>` (immutable — §2.1)。fsck は再ハッシュで検証し、
  tree entry の `normalize.manifest_hash` (§8) がこの object を指す ([10-operations.md §7.5.1](10-operations.md))。

**normalized_unit_object** — canonical JSON の content hash (§2.1):

```text
unit_object_hash = "sha256:" + base16(sha256(full NormalizedUnitObject の canonical JCS バイト列))
```

- 保存パスは `objects/normalized_unit_objects/ab/cd/<unit-object64>`。確定 manifest の done entry が
  この hash を必須で参照し、failed entry は明示 null とする。path-named current projection の bytes は hash
  入力でも歴史的な解決元でもない。

**chunk** — identity hash:

```text
chunk_hash = "sha256:" + base16(sha256(JCS({
  "spec_version": 1,
  "raw_hash": "...",
  "tool_profile_hash": "...",
  "gen": <int>,
  "unit_key": "...",
  "unit_content_hash": "sha256:...",
  "heading_path": ["...", "..."],
  "section_id": "...",
  "byte_start": <int>,
  "byte_end": <int>
})))
```

- `gen` は normalized unit の世代番号、`unit_key` は chunk が属する unit の識別子 (例 `page:12`)。`unit_content_hash = sha256(exact normalized Markdown bytes)` は same-gen retry の本文差を別 chunk identity にし、provider metadata / generated_at だけが異なる同一本文の re-ingest は同一 identity に保つ。full immutable unit の CAS locator は tree が pin する manifest の `units[].unit_object_hash` にだけ保持し、chunk object へ複製しない。`byte_start` / `byte_end` は **unit-local** の UTF-8 byte span。
- null / 未設定フィールドは hash 入力に含めない (§5.1 と同じ規則。`section_id` を持たない chunking strategy では省略)。
- chunk object 本体は `unit_content_hash` (exact Markdown bytes hash) を保持する。`unit_object_hash` は同じ Markdown でも provider metadata / generated_at により変わり得るため chunk へ保持しない (同一 chunk hash の object bytes を複数形にしない)。fsck / Evidence は tree → manifest → immutable unit CAS の閉包を検証し、そこで得た exact Markdown の hash を chunk の `unit_content_hash` と照合する。`text_hash` は unit content hash が既に本文全体を認証するため identity へ重複して含めない。

**embedding** — identity hash:

```text
embedding_hash = "sha256:" + base16(sha256(JCS({
  "spec_version": 1,
  "target_type": "chunk",
  "target_hash": <text_hash>,
  "profile_hash": <embedding profile_hash>,
  "modality": "...", "dimensions": <int>, "distance": "..."
})))
```

- cursor replay の query vector は embedding object / source SQLite row の identity ではない。device-local の `${XDG_CACHE_HOME:-$HOME/.cache}/kio/search-query-cache/<query_vector_digest>` に digest を名前として保存し、replay 時に同じ digest を再検証する。欠落・不一致は cursor を拒否するだけで、query を再 embedding したり scope の SQLite を読んだりはしない ([05-runtime.md §1.5](05-runtime.md))
- `target_hash` は対象 chunk の **`text_hash`** (chunk 抽出範囲のみの content hash、§8) であって `chunk_hash` ではない。embedding は Markdown 本文そのものの関数なので、同一本文を持つ複数 chunk (別世代・別ファイルの同一断片) は 1 本の `embeddings` 行を共有する — これが **content ベース再利用** ([04-pipeline.md §4.3 / §5.5](04-pipeline.md)) の identity 基盤である。`chunk_vec` (vec0) は `chunk_hash → embedding` の写像として `embeddings` から導出する。
- embedding object の保存 bytes は **`JCS(identity fields) + LF + base64(vector, float32 little-endian) + LF + lower_hex64(sha256(vector bytes))`** に固定する。fsck ([10-operations.md §7.5.1](10-operations.md)) は identity hash の再計算に加え、vector 長 (= dimensions × 4 bytes)・有限値 (NaN / Inf の拒否)・**vector digest の一致** (有限値への bit flip の検出) を検査する。

## tree / commit object

```json
// tree — objects/trees/3f/9a/<tree64> に JCS 形式で保存 (tree_hash は保存バイト列の sha256)
{
  "object_type": "tree",
  "entries": [
    {
      "path": "report.pdf",
      "type": "file",
      "raw_hash": "sha256:abc...",
      "normalize": { "tool_profile_hash": "sha256:tool1...", "gen": 0,
                     "manifest_hash": "sha256:mani..." }
    }
  ],
  "chunking_config_hash": "sha256:cfg..."
}
// current tree schema:
//  - entry.normalize.manifest_hash = 当該 normalized instance manifest の canonical hash
//    (JCS bytes の sha256 — §2.1)。unit の failed → done 遷移で変わるため、**derived 成果の変化が
//    tree_hash を変える** = same-gen partial retry の finalize も通常の auto snapshot を生む
//    ([05-runtime.md §8.1](05-runtime.md))
//  - tree.chunking_config_hash = snapshot 時点の effective chunking config (§5.3)。再 chunk も同様に
//    tree_hash を変える。必須であり、欠落は schema violation として fail-closed する
//  - manifest_hash は objects/manifests/ の immutable manifest object (§2.1) を指す — same-gen retry で
//    作業コピー manifest.json が更新された後も、過去 commit の manifest bytes と、その done unit が指す
//    normalized_unit_object CAS body はこの closure から解決できる
//  - 公開の authority は tree 上の集合 digest ではなく、chunks.jsonl の tagged publication event
//    `(chunk_id, chunking_config_hash, introduction_commit)` である。event の introduction commit は
//    その tree が同じ chunking_config_hash を束縛し、exact pinned normalized unit を認証できるときだけ
//    受理する。incomparable な introduction は独立に有効である。

// commit — objects/commits/9f/2c/<commit64> に JCS 形式で保存
{
  "object_type": "commit",
  "tree": "sha256:3f9a...",
  "parents": ["sha256:71bd..."],
  "created_at": "2026-04-29T12:00:00Z",
  "message": "snapshot after indexing docs",
  "tool_lock_hash": "sha256:...",
  "stats": { "files_added": 12, "files_modified": 3, "files_deleted": 1 },
  "commit_type": "manual"
}
```

JCS ではキー順は canonical 化時に自動決定されるため、上記の記載順は可読性のためのもの。

`commit_type=purged` の commit は **`purged_raws` (当該 purge の対象 raw_hash の昇順配列 — prepared 相の closure から確定、[05-runtime.md §3.5](05-runtime.md) の planned_commit) を必須 field に持つ**。marker 検証 ([10-operations.md §7.5.1](10-operations.md)) は tombstone / receipt の raw_hash がこの配列に含まれることを対照する — 他 raw の正当な purge commit を `in_commit` に流用した偽 marker が genuine missing を隠せない。他の commit_type は本 field を持たない。**本 field は store format の初版から必須であり、これを欠く `commit_type=purged` commit は存在しない** (欠落 = corruption。2026-07 の field 追加は実装・store 公開前の定義確定)。

tree entry の `normalize` ブロックは **optional**。normalized instance が存在しないファイル (未 Markdownize) の
entry では `normalize` を**省略**する (省略 = 当該ファイルの normalized / chunk は存在しない。`null` は書かない —
§5.1 の「省略と null を識別しない」に従う)。Markdownize されていない entry は省略形、
完了済み entry は `normalize` 付きである。

`normalize` が存在する場合、tree entry の `gen` は commit 時点で参照していた normalized instance の世代 (§2.1) であり必須である。欠落は schema violation として拒否する。`kio reindex --regenerate` 後も過去 commit の tree entry はその記録済み gen を
指し続けるため、`kio view --at` ([05-runtime.md §4.2](05-runtime.md)) と Evidence Pointer の
不変性保証 ([08-evidence-pointer-spec.md §6](08-evidence-pointer-spec.md)) は、gen ではなく
`manifest_hash → unit_object_hash` の immutable CAS closure により成立する。

`commit_type` は固定 enum (詳細は [05-runtime.md §2](05-runtime.md)):

```
manual | auto | repaired | purged | restored
```

commit object の schema 検証 (publication 時の loader — [05-runtime.md §2.1](05-runtime.md)。commit は CAS JSON object であり SQLite に commit 表は無い) でこの値域だけを受理する。未知値は schema violation として拒否し、legacy reader や値変換は持たない。

`restored` は現在 HEAD を唯一の `parent` とし、`restore_provenance` に
`source_commit` と復元対象の `paths`（重複なし・昇順）を必須で保持する。
過去の source は親参照ではない。`parent: null`、制御領域の path、および他の
commit type に付いた restore provenance は拒否する。履歴の path 検証は OS 非依存とし、
実際にファイルを作成するときに宛先 OS の名前制約を追加する。`restored` の tree は
`manual` と同様に GC の保持対象である。

## 8.2 tree のスケール前提 (flat entries)

tree は entries を単一の flat 配列で持つ。スコープ境界規則 (§3) により entry 数は scope フォルダ直下のファイル数に一致し、1 tree に階層は存在しないため、Git 式のディレクトリ単位 tree object (サブツリー hash 共有) は導入しない。

サイズ見積り (1 entry ≈ 150-250 bytes):

| 直下ファイル数 | tree object サイズ | 備考 |
| --- | --- | --- |
| 100 | 約 25 KB | 典型的なフォルダ |
| 1,000 | 約 250 KB | 大きめの Downloads 等 |
| 10,000 | 約 2.5 MB | 想定上限 (soft limit) |

規範:

- 1 scope の直下ファイル数の想定上限は 10,000 (soft limit)。超過時 `kio index` は警告を表示し、サブフォルダへの分割または ignore を提案する (処理自体は継続する)
- snapshot 時に tree_hash が現在の HEAD の tree と一致する場合、auto snapshot は commit を作らない (no-op。**例外 = resurrection finalize と tool_lock_hash の変化** — no-op 判定は tree_hash と commit の tool_lock_hash の両方を比較する。正本 [05-runtime.md §8.1](05-runtime.md))。tree は CAS object なので、内容不変なら新規 object も生成されない (tree_hash は保存バイト列の content hash、§8.1)
- 1 ファイルの変更で tree 全体 (上表のサイズ) が新 object として書かれるのは仕様どおりの挙動である。

## chunk

```json
{
  "spec_version": 1,
  "raw_hash": "sha256:abc",
  "tool_profile_hash": "sha256:tool1",
  "gen": 3,
  "unit_key": "page:12",
  "unit_content_hash": "sha256:markdown...",
  "heading_path": ["認証仕様", "API Token"],
  "section_id": "認証仕様/api-token",
  "byte_start": 1200,
  "byte_end": 1500,
  "text_hash": "sha256:text",
  "text": "chunk の exact normalized text"
}
```

chunk identity は `(raw_hash, tool_profile_hash, gen, unit_key, unit_content_hash, heading_path, section_id, byte_start, byte_end)` で決まり、chunk_hash の算出式は §8.1 に定める。`unit_content_hash` は exact normalized Markdown bytes の hash。full immutable object の `unit_object_hash` は manifest entry が保持し、fsck/Evidence が tree から closure を認証する。chunk object へは複製しないため、metadata だけが異なる同一本文でも同じ chunk identity の CAS bytes は一意である。heading_path と section_id は両方 hash 入力、未設定フィールドは省略する。**`byte_start` / `byte_end` は unit-local の UTF-8 byte offset・0-based half-open**。

chunk object の永続 JSON は上記の `spec_version` + identity fields + `text_hash` + exact `text` に固定し、
自身の `chunk_hash`、path、`created_at`、`chunking_config_hash` は含めない。
`chunking_config_hash` は同一 chunk identity に複数値が対応しうる generation association として
**append-only の `chunks.jsonl` (§2 layout / §4.1 — truth) を正本に、SQLite の別 relation (cache) へ**保持する。fsck は object bytes の content hash ではなく
§8.1 の identity hash を再計算して保存 fan-out key と照合し、`text_hash` を object 内の `text` と
対応 normalized unit の exact span の両方に照合する。

## 8.3 Unreachable-object inventory graph

Phase 4 milestone 8 の read-only inventory は、`.kio/objects/` の次の physical namespaceだけを
CAS object kindとして列挙する。

```text
commits, trees, raw, chunks, manifests, normalized_unit_objects,
embeddings, toollocks, prepared, image
```

mutable projection/cacheである `objects/normalized/` と `objects/normalized_units/` は物理CAS objectの
件数・bytes・candidateに含めない。未知namespace、非canonical fan-out、hash leaf以外はcorruptionである。

到達edgeは次の正本だけから構成する。SQLite table/rowはedgeを追加も削除もしない。

```text
HEAD / refs/heads/* / refs/tags-v1/tag-* → commit
commit.parents                         → commit
commit.tree                            → tree
commit.tool_lock_hash                  → toollock
tree.entries[].normalize.manifest_hash → manifest
manifest.units[status=done].unit_object_hash → normalized_unit
embedding(target_type=chunk).target_hash     → existing chunk.text_hash
embedding(target_type=image).target_hash     → existing image hash
```

commitはref未到達でもappend-onlyであり、その `tree` / `tool_lock_hash` edgeを保持する。物理treeは
既存retention GCの専管なので、commitから未参照でもそのnormalize edgeを安全側で保持する。
shallow receiptは対応commitのtree欠落だけを説明し、別objectへの削除権限を与えない。一つでも
validなshallow receiptがあれば欠落treeのclosureを復元できないため、現在のtreeから未参照の
manifestと現在のmanifestから未参照のnormalized unitは `inventory_only /
shallow_history_unavailable` とし、参照ゼロと推定しない。

完了済みpurgeは履歴treeを書き換えずraw/derived closureを物理消去するため、厳格に検証したcanonical
final lifecycle eventだけがhistorical closure欠落を説明できる。`purged` / `erased` はeventの `in_commit`、
`retired` はverified resurrection commitをanchorとし、そのanchorに対する**strict ancestor commit**が参照する
treeのclosureだけを説明する。anchor自身・その子孫・無関係branch・commitから未参照のtree・marker不整合は
説明対象外でありfail-closedする。欠落manifestのunit pinは復元不能なので、同じraw/profile/gen identityを持つ
存置normalized unitは参照ゼロと推定せず `inventory_only` に保護する。
markerの構造、leaf identity、event列、epoch、`in_commit`のref到達性・`commit_type=purged`・
`purged_raws` membership・`created_at`、retirement ancestryと存置treeのraw membershipを検証する。
各 marker record の `lifecycle_epoch` は正の値かつ event 順に厳密単調増加でなければならず、
重複・逆行した record は historical closure 欠落を説明できない corruption とする。
active journal / GC recovery / writer、counter不整合、またはこの因果関係を確定できない状態では候補を出さない。

物理bytesはverified regular fileの実サイズであり、summaryの `physical_bytes` は全object行の総和、
分類別bytesの総和も必ずこれに一致する。canonical hashは常に完全な `sha256:<64 lowercase hex>`。
reportはpath、元filename、actor、reason等のlifecycle内容、secretを公開しない。

# 9. Dedup スコープ

```
dedup scope            = one .kio object store
cross-.kio dedup       = not guaranteed
cross-.kio GC scope    = none (各 .kio に閉じる)
```

per-`.kio` の prepared/normalized/embedding 重複と purge の `.kio` 単位スコープは、
local-first の分離コストとして受け入れる。

# 10. 書き込み主体マトリクス

```
レイヤー                       | User | Kio
------------------------------ | ---- | ---
原本 (raw)                     | yes  | no
原本の移動 (file system mv)     | yes  | no
normalized markdown            | no   | yes
image objects (抽出画像)        | no   | yes
chunks / embeddings            | no   | yes
commits / refs (履歴)           | no   | yes
extraction issues              | yes  | yes
```

normalized (unit object および全文 view) は **read-only artifact**。全文 view の生成時に付与する
Markdown ヘッダ template (Source の filename も comment-safe に挿入する — `--` を含む名前は
percent-encode、§2.1 の KIO-MISSING-UNIT と同じ規則。生値の挿入は comment を途中終端させ view 冒頭へ
任意 Markdown を注入できてしまう)。**Source の値は生成時点の filename の記録**であり
first-instance-wins の一部 — 同一 (raw_hash, tool_profile_hash) を別 path・別名で再配置しても
view は再生成しない (現在の path 表示は view 提供側 (CLI 表示層) の責務で、cache 本文は不変)。
**view 喪失後の再生成 (`kio repair`) では Source に再生成時点の filename を用いてよい** — view の
content は identity を持たない (§5 の normalized_hash 不採用) ため、Source 行は informational で
あり不変条件ではない:

```markdown
<!--
Kio GENERATED FILE
Do not edit manually.
Source: report.pdf
Raw-Hash: sha256:...
Tool-Profile-Hash: sha256:...
Generated-At: 2026-04-25T12:00:00Z
-->
```

path-named current projection の直接編集は hash 検証で破損検出しない (projection 自体は content hash を持たないため)。次回 loader は `unit_object_hash` CAS からこれを再 materialize する。immutable NormalizedUnitObject CAS の変更・欠落は再 hash で検出し、過去 commit の内容を same-gen で差し替えない。全文 view (`objects/normalized/*.md`) も cache のため、直接編集は次回 view 再生成で破棄される。

# 11. 設定ファイル

`~/.config/kio/tools.toml` (デバイスローカル, 共有 `.kio` には含まれない):

```toml
[markdown.mistral_ocr_markdownize]
kind = "online_api"
model = "mistral-ocr-latest"        # config では可変 alias 可。tool_profile の pin は解決済み immutable 版 (§5.1)
profile_hash = "sha256:..."
capabilities = ["ocr", "layout_detection", "table_extraction"]

[markdown.mistral_ocr_markdownize.pricing]   # 単価の正本 (07 §4 billable_units の換算元 — tool-lock ではない)
pages = 0.004                                # unit kind → USD 単価。換算は終端 Tx 時点の表で確定 (07 §4)

[embedding.gemini_embedding.pricing]
tokens_in = 0.00000015
```

現行版の認証付き Markdownize / Embedding adapter は Kio 組込み target のみを実行する。`cmd` / `args` / `url` による任意 target は受理しない。

`.kio/config.toml`:

```toml
[scope]
participates_in_global_search = true
[chunking]
strategy = "heading"
max_chars = 6000
# max_chars の計数単位 = Unicode scalar value (code point)。分割規則は 04-pipeline.md §4.1
# [chunking] の変更は chunking_config_hash (§5.3) の変化として検出され、
# chunk / embedding のみ再生成される (再 Markdownize しない)。規則は 04-pipeline.md §4.6
unicode_version = "17.0.0"  # slug 正規化の UCD 版 (§5.3 — 省略不可・default なし。実装が同梱する UCD 版と一致させる)
[markdownize.incremental]
enabled = true
threshold = 0.30
max_consecutive = 5
[adapter]                  # online 送信レーン (07 §5.3 の 2026-07-24 裁定)
lane = "batch"             # "batch" (既定・半額) | "realtime" (即時・単価 2 倍)
                           # OCR と embedding の**両方**に同時に効く — 片方だけ別レーンにはできない。
                           # 解決順は network opt-in と同形: CLI (--realtime/--batch) > 本キー
                           # (scope) > user config の同キー > 既定 (batch)。
[budget]                   # folder cap (任意の追加制限)。device cap との判定は 04-pipeline.md §5.4
monthly_usd_cap = 10.0
[gc]
mode = "on_idle"               # 明示 opt-in。省略時は manual_only (05-runtime.md §2.3)
max_runtime_seconds = 60         # 1..86400
idle_threshold_seconds = 300     # 1..31536000
```

すべての設定は JSON Schema/TOML Schema で validate ([10-operations.md §11.3](10-operations.md))。
`[gc]` を置かない場合だけ `manual_only` が既定となる。`[gc]` がある場合は `mode` が必須で、
`manual_only` は `idle_threshold_seconds` と `max_runtime_seconds` を禁止、`after_index` は
`max_runtime_seconds` を必須・`idle_threshold_seconds` を禁止、`on_idle` は両方を必須とする。

## 11.1 .kioignore 文法 (2026-07-03 追記)

`.kioignore` は scope ルート (`.kio` と同階層) に置く。`kio index` の探索全体 — 直下ファイルの取り込み判定と、サブフォルダへの子 `.kio` 自動生成の対象判定 ([06-cli-spec.md §1](06-cli-spec.md)) — に適用する。文法は **gitignore 互換サブセット**:

```text
# で始まる行と空行     無視 (コメント)
glob                  * (パスセグメント内) / ** (セグメント跨ぎ) / ? (1 文字)
末尾 /                ディレクトリのみに一致
!pattern              negation (直前までの除外を解除)
評価順               上から順に評価し、最後に一致した規則が勝つ (後勝ち)
パス基準             scope ルート相対
```

secrets built-in デフォルト除外 (Tier A、[10-operations.md §1.1](10-operations.md)) は `.kioignore` の**暗黙の先頭**に位置するとみなす。したがって Tier A の pattern としての解除は明示の `!pattern` でのみ可能で、解除時の確認記録は 10 §1.1 の規約に従う (10 §1.1 の「対話承認時の個別選択」は当該 raw_hash の一回性取り込みとして完結するものであり、pattern の解除ではない — 持続する解除経路は `!pattern` のみ)。config (`[scope] ignore`) と `.kioignore` が両方ある場合は config → `.kioignore` の順に連結して評価する (後勝ちのため `.kioignore` が優先)。
