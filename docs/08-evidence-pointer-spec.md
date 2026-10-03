# 08 Evidence Pointer Spec

Kio の中核概念 **Evidence Pointer** の正式仕様。外部 AI Agent や他ツールが Kio と相互運用する際の契約となるため、独立 spec として保持する。

> 関連: [03-data-model.md §1, §5](03-data-model.md) (CAS / identity) / [05-runtime.md §3](05-runtime.md) (purge / Dead Pointer) / [02-philosophy.md](02-philosophy.md) (なぜ Evidence Pointer か)

---

# 1. なぜ Evidence Pointer か

通常のファイル検索ツールは「path + 行番号」で根拠を指す。これは:

- ファイル移動・リネームで死ぬ
- 削除で死ぬ
- 上書き保存で意味が変わる
- 過去版に戻れない

Kio は **path ではなく content-addressed object** で根拠を指すことで、これらの脆弱性を排除する。

```
通常:  "report.pdf:42"                  → 移動・リネーム・削除で死ぬ
Kio:   commit + raw_hash + chunk_hash   → ファイル移動・リネーム・削除に耐える
       + path_at_commit + span            (purge されない限り永続)
```

これは Kio の差別化の中核 ([01-positioning.md](01-positioning.md))。

---

# 2. Evidence Pointer のスキーマ

```json
{
  "schema_version": 1,
  "commit": "sha256:9f2c...",
  "tree": "sha256:3f9a...",
  "raw_hash": "sha256:abc123...",
  "tool_profile_hash": "sha256:tool1...",
  "chunk_hash": "sha256:chunk456...",
  "path_at_commit": "report.pdf",
  "heading_path": ["認証仕様", "API Token", "有効期限"],
  "section_id": "認証仕様/api-token/有効期限",
  "byte_start": 1200,
  "byte_end": 1500,
  "scope_id": "scope_01J8ZQ...",
  "scope_path": "/Users/foo/Research/.kio"
}
```

`path_at_commit` は **commit 時点の scope フォルダ直下でのファイル名** であり、パス区切り (`/`) を含まない ([03-data-model.md §3](03-data-model.md))。区切り等を含む path は表示専用としても受理せず schema violation として fail-closed にする。フォルダ階層上の位置は `scope_path` が示す。人間向け表示ではこの 2 つを組み合わせて表示する (§7.2)。

## 2.1 必須フィールド

| フィールド | 役割 | 不変条件 |
| --- | --- | --- |
| `schema_version` | Evidence Pointer schema の version | 現在は `1` のみ受理 |
| `commit` | commit object の content hash (commit_hash, [03-data-model.md §8.1](03-data-model.md)) | append-only。GC (shallow 化) でも失われない |
| `raw_hash` | 原文バイト列の identity | 移動・リネームで不変 |
| `tool_profile_hash` | Markdownize Adapter capability の identity | tool 変更で別 chunk に飛ばない保証 |
| `chunk_hash` | chunk object の identity | `(raw_hash, tool_profile_hash, gen, unit_key, unit_content_hash, heading_path, section_id, byte_start, byte_end)` から導出。本文変更は分離し、同一本文の再取り込みは identity を維持 ([03-data-model.md §8.1](03-data-model.md)) |
| `scope_id` | 正本 `.kio` の path 非依存 identity (`.kio/scope.json` 保持) | `.kio` の移動で不変 |

## 2.2 Optional フィールド

| フィールド | 役割 |
| --- | --- |
| `tree` | 当該 commit の tree_hash (高速解決用。shallow 化済み commit では tree object 自体は存在しないことがある) |
| `path_at_commit` | commit 時点の正確な tree entry を指定する optional selector。指定時は別名で代替せず、現在の policy も検証する |
| `heading_path` / `section_id` | chunk の構造的位置 (UI 表示用) |
| `byte_start` / `byte_end` | normalized unit 本文内の UTF-8 byte span (unit-local・0-based half-open、[03-data-model.md §8.1](03-data-model.md)) |
| `scope_path` | 生成時点の正本 `.kio` の絶対パス (解決の高速ヒント + 表示用。解決の root 信頼は `scope_id`) |

`heading_path` がない chunk は、wire form で `heading_path` field 自体を**省略する**。field を
present にする場合は、1〜64 個の非空文字列だけを順序どおり格納する。`"heading_path": []` と
`"heading_path": null` は non-canonical であり、production parser と verifier は受理しない。

表示用 field (heading / span) は、解決が成功した場合は**解決結果の canonical 値 (tree / chunk object 由来) を優先して
表示し、pointer 入力値と相違するときは入力値を無視する** — 正しい必須 tuple に偽の表示 metadata
(heading / span) を付けた pointer が、alive 判定のままそのまま人間向け引用に使われることを
防ぐ (これらは解決には元々使わない — §3.1 手順 8 の整合検証は必須 tuple のみ)。receipt により
正当化された tree 欠落では、`kio open` / `kio view` は本文・表示 metadata を返さず
`KIO-E-COMMIT-SHALLOW-001` で終端する (§3.1 手順 2a)。pointer 入力の `path_at_commit` で代替表示したり、
raw / chunk だけから本文を返したりしない。

`path_at_commit` を指定した本文解決は、その commit の正確な entry と raw / profile / generation / manifest
の帰属を検証する。指定した path が Ignore 対象なら、同じ内容の許可された別名があっても拒否する。
省略された URI は、同じ immutable identity に帰属し、現在の policy で許可された別名から決定論的に
選択する。commit 自体も現在の管理対象 HEAD の履歴から到達できる必要がある。CAS にコピーされた
object や古い SQLite 行だけでは本文の読み出しを許可しない。

`scope_path` は **ヒント** であり、解決の root 信頼にしない。移動後も scope identity と現在の管理境界が
検証できれば解決可能である。管理対象の子の移動・コピーは、再登録なしに以前の承認を引き継がない。

## 2.3 正規シリアライズ (canonical serialization)

Evidence Pointer の交換形式は 2 つ。**完全形は §2 の JSON object**、**正規テキスト形は以下の URI** とする。

```text
kio://<scope_id>/<commit>/<raw_hash>/<tool_profile_hash>/<chunk_hash>[?sv=<schema_version>]

例:
kio://scope_01J8ZQ.../sha256:9f2c.../sha256:abc123.../sha256:tool1.../sha256:chunk456...
```

規則:

- URI は **必須フィールドのみ** を持つ。optional フィールドは URI ⇄ JSON の往復で失われる。
  `path_at_commit` の省略は §2.2 の別名選択を意味し、元の JSON の明示 path 選択を保存する形式ではない。
- `sv` (schema_version) 省略時は `1`。`1` 以外の `sv` は KIO-E-CONFIG-SCHEMA 系 error (exit 2)。
  **URI は opaque として扱い、authority 位置
  (scope_id) の大文字小文字を保存する** — 一般 URI 正規化 (authority の小文字化) を適用してはならない。
  lookup は case-sensitive (registry の TEXT キーと一致 — ULID は大文字表記が正)。
- 各セグメントは §2 の同名フィールド値をそのまま置く (hash は `sha256:` prefix 込み、commit は commit_hash、
  [03-data-model.md §8.1](03-data-model.md))。percent-encoding は不要 (値域が `[A-Za-z0-9_:.-]` に閉じるため)。
- 第 2 セグメントがリテラル `object` の URI は **object 参照**であり、Evidence Pointer ではない:
  `kio://<scope_id>/object/<type>/<hash>` (例: normalized view 内の画像参照
  `kio://<scope_id>/object/image/<image_hash>`、[07-adapter-spec.md §5.2](07-adapter-spec.md))。
  `kio open` はこれを受理して該当 object を解決する (**MVP で発行・受理される object URI は
  type=image のみ** — 発行面は [07-adapter-spec.md §5.2](07-adapter-spec.md) の画像参照置換だけで、
  他 type の URI は発行されない。受理側も image 以外は拒否 — [06-cli-spec.md §1.1](06-cli-spec.md)
  手順 1a。type を追加する場合は 06 §1.1 に open semantics を定義してから)。Evidence Pointer URI の
  第 2 セグメント (commit) は
  常に `sha256:` prefix を持つため、リテラル `object` と衝突しない。

CLI の `<pointer>` 引数はすべて以下の受理規則に従う (優先順位順に prefix で判定):

```text
1. "-"          stdin から 1 つ読む (JSON object または URI 1 行)
2. "kio://"     URI 形
3. "{"          inline JSON (§2 schema)
4. "sha256:"    短縮形 (kio open / kio view のみ): object store を照会して種別を判別し、
                chunk_hash なら chunk、raw_hash なら raw として、カレント .kio + HEAD を
                文脈に解決する。raw_hash と chunk_hash の両方に該当し多義なら
                KIO-E-EVIDENCE-SCOPE-AMBIGUOUS-001 (exit 2) を返す
5. その他       parse 失敗 → exit 2 (invalid usage)
```

bulk 系 (`kio evidence verify --batch <pointers.jsonl>`) は従来どおり各行 JSON object。

## 2.4 識別子の安定性と引用の保持

引用の識別子が不変であることと、原本・本文をいつでも解決できることは別である。
auto / repaired commit の tree は retention GC で shallow 化され得る。raw / chunk が残っていても、
必要な tree がなければ本文解決は §3.1 手順2aで拒否する。purge / erase、scope喪失、破損、
現在のIgnore・policyによる拒否も、解決可能性に影響する。

現行の `kio tag <name> <commit>` は同じchain内の保持されたcommitへ名前を付け、HEAD/tag tipを
retention GCのshallow候補から除外する ([06-cli-spec.md §6.1](06-cli-spec.md))。
重要な引用を保持する場合は、そのpointerが示す正確なcommitをshallow化される前にtag対象とする。
最新状態のmanual snapshotや復元commitのprovenanceは、元pointerのcommitを保護する代わりにならない。
既にshallow化されたcommitへのtag作成は拒否され、tagは明示purge/eraseを防ぐ保証でもない。

2026-10-03の開発版でtagの削除・同名再作成をCLIへ接続した。解除後は、そのcommitを保護する
別のrefがなければ通常のretention規則が再び適用される。名前の監査行と既存CASは削除時に保持する。
tag名は内容固定の引用識別子として使わず、既発行pointerをtagや最新版へ黙って付け替えない。
既存exportは原本ファイルの書き出しであり、別環境でpointerを検証・解決する自立bundleとは異なる。

2026-10-03承認の追加方針では、既存tagを利用した引用保持と削除影響の表示を先に改善する。
引用群の登録・自立bundle・文書lineageは詳細契約を別途固定する。
実装順序と受入条件は [実装計画](../tasks/knowledge-ux-implementation-plan-2026-10-03.md) を参照する。

---

# 3. Evidence Pointer の解決

```
入力: Evidence Pointer
出力: { raw_object | normalized_unit | chunk_text } または error
```

本節の「解決成功」は本文を返す `kio open` / `kio view` の resolver を指す。本文を返さない
`kio evidence verify` は §4.3 の status semantics を使い、receipt-backed shallow の観測をこの本文解決の
成功へ読み替えない。

## 3.1 解決手順

```text
1.  scope の解決 (2 段):
    a. scope_path が指定され、その .kio の scope.json の scope_id が pointer と一致 → それを使う —
       **ただし「validated scope_path の canonical path ∪ registry の live 行」の重複除去済み候補が
       2 以上の場合は 1a でも選択せず、1b と同じ候補一覧 error とする** (scope_path は表示用 hint で
       ありユーザーの明示選択ではない。registry 未登録 clone の path 指定でも、既知 live 行と合わせて
       2 以上なら同じ error — URI ⇄ JSON の表現差で alive / error が変わることを防ぐ —
       [10-operations.md §3](10-operations.md))
    b. 一致しない・存在しない・scope_path 省略 → scope_registry を scope_id で照会し kio_path を得る
       (同一 scope_id が複数 **live** 登録されている場合は選択しない — `KIO-E-REGISTRY-DUP-001` の
       候補一覧 error で fail-closed とし、dedupe を要求する ([10-operations.md §3](10-operations.md))。
       purge 状態の異なる clone へ黙って解決すると scope 単位 purge の判定を取り違えるため。
       **候補集合は registry の live 行に加えて validated scope_path の canonical path を含めて数える** —
       registry 未登録の clone を scope_path で指した場合も、既知 live 行と合わせて 2 以上なら同じ error
       (URI 化で optional path が落ちた場合と結果を変えない))
    c. どちらも失敗 → KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001 (scope_unreachable, §3.2)
    (1a/1b の「表現差で変えない」の対象は既知候補集合に対する判定 — scope_path が registry 未登録の
    clone を新たに教える場合に候補が増えて error 側へ倒れるのは fail-closed の意図どおりの**情報差**で
    あり、表現差ではない)
    EvidenceReadOnly は **1 resolver invocation 内で** registry main / `-wal` / `-shm` の 3 leaf を
    開始・終了で同一性まで観測した private owned snapshot を 1 回だけ開き、その snapshot の live 行を
    1a/1b の候補 union と後続の format peek へ再利用する。source leaf を SQLite に渡さず、private copy へは
    main と存在する WAL だけを渡す（SHM は copy しない）。これは formal な cross-file atomic
    snapshot を主張せず stable-or-fail の線形化点である。main / `-wal` / `-shm` の **全 3 leaf が absent**
    の Missing registry のみ cache miss として hint / CWD 規則を継続する。main absent + sidecar present は
    unsafe integrity とする。unsafe leaf / 上限超過 / usable identity欠如 / private owned copyの
    identity・内容不一致、または source 再観測が一致した owned copy の SQLite integrity failure は
    `KIO-E-REGISTRY-SNAPSHOT-UNSAFE-001` (exit 4)。regularなsource leafのpre/open/post identity・size変化は
    bounded retryで再観測し、不安定なcopyを採用しない。source presence/identity/size/hash drift /
    busy / retry exhaustion / temp I/O / integrity を確定できない private snapshot
    read-open-query failure は `KIO-E-REGISTRY-SNAPSHOT-001` (exit 3) で fail-closed し、
    hint / CWD fallback、nested scope status、batch_changed へ変換しない。
2.  commit を refs / objects/commits/ から取得し、現行管理scopeのHEADから到達可能であることを検証する。
    以下の本文解決では、過去のpolicyではなく現在の所属・Ignore・policyを使う。
2a. commit の tree が欠落し、有効な `.kio/gc/shallowed/<commit64>` receipt がその欠落を
    正当化する場合、本文を返す `kio open` / `kio view` は **`KIO-E-COMMIT-SHALLOW-001` (exit 1)**
    で fail-closed する。tree entry の membership / profile / gen / 時点帰属を検証できないため、
    raw_hash・chunk_hash・current HEAD・SQLite cache のいずれも代替 authority にしない。回復は backup
    から当該 tree object を復元して完全な tree 検証を再実行することだけである。本文を返さない
    `kio evidence verify` の shallow 観測は §4.3 の別 status semantics に限る。
3.  tree (commit.tree) を取得
4.  tree の raw_hash と tool_profile_hash が一致する正規化済みentryを選ぶ。
    path_at_commit が指定されていれば、そのpathだけを検証し、別名の許可で代替しない。
    省略時は、同じbindingのうち現行policyで許可されたentryをpathのUTF-8 byte順で選ぶ。
    各entry自身のgen・manifest・normalized unitを検証し、他entryのmanifestを借用しない。
    genの整合は手順8でchunk objectとも検証する。現行policyで拒否されたpathしかなければ
    KIO-E-EVIDENCE-POLICY-001で本文解決を拒否する。一致bindingがなければ
    KIO-E-STORE-CORRUPT-001で終端する。normalize=nullのbare snapshotは意味的なEvidence Pointerの
    authorityにならず、別commitの正規化結果を流用しない。bare snapshotのcurrent HEADにあるrawは、
    別契約のraw short hashで現行policyを確認した上で開ける。
5.  raw_hash の marker と raw object の存在を判定する。**まず、存在する全 marker (tombstone /
    erase receipt) の最終 event を 1 つに正本化する** — canonical final event = 全 marker 中で
    `lifecycle_epoch` 最大の最終 event ([05-runtime.md §3.5](05-runtime.md)。`lifecycle_epoch` を欠く
    event は malformed current record であり corruption とする。同値は tombstone 側を優先する決定的 tie-break。resurrection link も canonical
    final event のものを採用する)。**正本化の入力は event 検証 (kind 別必須 field・遷移文法・
    `in_commit` / `purged_raws` membership / `at` — [05-runtime.md §3.5](05-runtime.md) の validity、
    正本は [10-operations.md §7.5.1](10-operations.md)) を通過した marker のみ** — 検証失敗の marker は
    `KIO-E-STORE-CORRUPT-001` で終端し、canonical 判定に参加させない (fsck と resolver で扱いを
    割らない)。**以下 (i)〜(iv) は canonical final event に対して評価する**
    (§3.2 の解決成功条件「raw object が存在」をここで検査する — (i) が個別 marker の末尾で先に
    短絡しない: 例えば tombstone 末尾 purged@epoch10 + receipt 末尾 retired@epoch11 は canonical =
    retired であり (iii) 側):
    (i) canonical final event = `purged` (active な tombstone) なら → tombstone を返す (§4)。
    (ii) canonical final event = `erased` (active な erase receipt) で raw object が不在なら not_found —
    `KIO-E-PURGE-NOT-FOUND-001` (§4.2 の表と同一の終端)。
    (iii) canonical final event = `retired` なら tombstone 扱いしないが、手順 6 へ進む
    **前に raw object の存在を検査する** — 存在すれば手順 6 へ進む (resurrection 後の旧 pointer を
    alive に戻すための必須条件)。**不在なら not_found — `KIO-E-STORE-CORRUPT-001`**
    (retired 後の再作成分の欠落は corruption — [10-operations.md §7.5.1](10-operations.md) と整合。
    chunk object が残存していても本文を返さない)。
    (iv) marker (tombstone / erase receipt) が無いのに raw object が不在なら not_found — code は
    `KIO-E-STORE-CORRUPT-001` (marker なしの欠落は
    purge の痕跡ではなく **corruption の疑い** — 手順 4 の短絡と同じ not_found 扱いで返し、
    `kio repair verify-objects` を案内する。purge 済みの正規欠落 (marker あり) と混同しない)。
    **(i)〜(iv) のいずれにも該当しない場合** (marker が無い・または active な erase receipt が
    あっても raw object が存在する場合を含む) は raw object が存在する通常状態であり、手順 6 へ進む
6.  tree entry の normalize.(tool_profile_hash, gen) と `manifest_hash` で normalized instance を解決する。
    historical / `--at` / Evidence 解決は manifest CAS の該当 done entry の non-null `unit_object_hash` から
    immutable NormalizedUnitObject CAS を読む。path-named `normalized_units/` の current body や、同 gen の
    後から更新された最新 body を読む経路はない。
    `normalize` が存在する entry で gen が欠落する場合は current schema violation / corruption として
    fail-closed にする (gen=0 へ補う reader は置かない)。
6a. **時点帰属の検証 (v2 tree)**: entry の normalize.manifest_hash が指す manifest object を読み、
    chunk の unit_key が当該 manifest で status=done かつ non-null `unit_object_hash` を持つことを検証し、
    当該 hash の NormalizedUnitObject CAS から exact body を得て、その Markdown hash が chunk object の
    `unit_content_hash` と一致することを検証する (unit_key は chunk_hash から chunk object の header を読み取って得る — 手順 7 の本文取り出しに先行する read-only 参照) — done でない unit の
    chunk は当該 commit 時点に存在しない (same-gen retry の後着 chunk を過去 commit の証拠として
    返さない → not_found)。**v2/v3 tree ではさらに、chunk の publication と config association の
    introduction ([04-pipeline.md §4.1](04-pipeline.md)) が pointer の commit の ancestor-or-equal で
    あることも検証する** (config association は**対象 tree の `chunking_config_hash` のもの** —
    [05-runtime.md §1.6](05-runtime.md) の検索側と同一の絞り込み。別 config の association は当該
    commit への帰属を証明しない) — manifest で done でも当該 commit 時点で未公開の chunk を証拠にしない
    (cache 参照のため、association の**不在**による失敗は corruption ではなく not_found — rebuild 後に
    再評価できる。**sqlite.db 自体の不在・再構築中はこの検証を実行できない — not_found ではなく
    `KIO-E-INDEX-REBUILDING-001` の再構築要求を返し ([05-runtime.md §6](05-runtime.md))、検証不能を
    「不在の確定」と混同しない**)。
6b. entry の manifest object が purge により欠落している場合 (raw_hash の **tombstone または
    erase receipt** の lifecycle — active / retired を問わず — が説明する欠落。**説明範囲は fsck と
    同一** ([10-operations.md §7.5.1](10-operations.md)): 当該 purged / erased event の `in_commit`
    **以前**の commit が参照する closure に限る — pointer の commit がこの範囲外 (retire 後に再作成・
    再公開された manifest の欠落) なら 6b を適用せず KIO-E-STORE-CORRUPT-001 (not_found 扱い) とする。
    古い marker が新規破損を隠さない): 手順 2a と同じ
    直接解決へ降格し、レスポンスに `manifest_missing: true` を付す。**ただし 2a と異なり 6b は
    手順 4 の tree entry を取得済みであり、手順 8 の entry 系照合 (normalize.tool_profile_hash の
    pointer 一致・gen 一致) は実施する** (降格するのは manifest 依存の検証のみ)。**retired event に
    `resurrection_commit` があれば、そのリンク先 commit の publication を参照して本文を解決し
    alive を返してよい** ([05-runtime.md §3.5](05-runtime.md) — 検索の時点条件には影響しない)。
    時点帰属は検証できないため --strict verify は
    unverifiable (reason = manifest_missing は恒久のため exit は 4 — §4.3) — 再 ingest 後の
    manifest は run_id 等が異なり旧 hash を再生できない
    ([03-data-model.md §2.1](03-data-model.md)) ので、この降格は恒久である。**6b でも 6a の v2/v3 検証
    (publication / association の introduction ancestry) は実施する** — cache は manifest と独立に
    参照でき、失敗 = not_found。ただし**基準 commit は経路で異なる**: リンクを使わない直接解決は
    従来どおり pointer の commit を基準にする (purge → 再 ingest 後の後着 chunk を旧 commit の
    証拠にしない)。**resurrection link 経由の解決は、当該 retired event の `resurrection_commit` を
    基準に検証する** — リンク先 commit が当該 chunk の publication / config association の
    introduction を ancestor-or-equal に持つこと (再 ingest の publication は旧 pointer commit の
    後続にあるため、旧 commit 基準ではリンク経路が恒久に不達になる)。resurrection で alive に
    戻るのは、リンク先 commit 側で**同一 chunking config** の下に同一 chunk が再公開された場合に
    限る — config が変わって chunk 境界が消えた場合の not_found は正 (境界非互換の物理的帰結で
    あり、alive 保証の破れではない)。リンクとして有効なのは
    **canonical final event (手順 5) が `retired` の場合の当該 event のみ** (個別 marker の末尾
    retired では不十分 — 別 marker により再 purge 済み = canonical が `purged` なら
    手順 5 で tombstoned)。リンク先 commit が不在・ref 不達、または上記検証に失敗した場合は
    リンクを使わず直接解決の規則へ戻る (それも失敗なら not_found)。
    `manifest_missing` は 6b を実行できる full-tree 解決でのみ設定される。receipt-backed shallow
    は手順 2a で本文解決を終端するため、この `manifest_missing` 降格経路へ入らない。
7.  chunk_hash で chunk object を解決し byte_start/byte_end の text を取り出す
8.  **整合検証**: 解決した chunk object の raw_hash / tool_profile_hash が pointer の値と一致し、
    手順 4-6 を経た場合はさらに **tree entry の normalize.tool_profile_hash が pointer の
    tool_profile_hash と一致し**、chunk object の gen が tree entry の gen と一致することを検証する
    (手順 4 の tool 一致選択とは独立に、終端で entry 側の tool 一致を再検証する defense-in-depth —
    この postcondition を欠くと、手順 4 の選択が破損・改変した store 上で迂回された場合に、同一 raw を
    別 tool で normalize した commit に対して gen 値の偶然一致 (双方 0 等) だけで別 tool の chunk が
    当該 commit の証拠として通ってしまう)
    (pointer は gen を持たない — gen の照合対象は tree entry と chunk object 内部のみ)。不一致は
    store corruption として KIO-E-STORE-CORRUPT-001 (not_found 扱い) — cross-wired な pointer が
    別文書の本文を「解決成功」として返すことを防ぐ。receipt-backed shallow は手順 2a で本文解決を
    終端するため、この tree membership 検証を省略して成功する経路はない。
```

## 3.2 不変条件

```text
解決成功条件:
  - scope (.kio) に到達できる
  - commit object と、その commit が参照する tree object が存在
    (commit object は GC で削除されない。receipt-backed tree 欠落は本文解決の `KIO-E-COMMIT-SHALLOW-001`)
  - raw object が存在 (purge されていない)
  - chunk object が存在 (= 同一 tool_profile_hash で生成済み)

部分的失敗 (代表例 — status union の正本は §4.3):
  - purged raw_hash (canonical final event = `purged` — §3.1 手順 5): tombstoned — tombstone を返す (§4.1)
  - scope 解決の重複 (validated ∪ live 候補 ≥2): registry_duplicate
                                        — KIO-E-REGISTRY-DUP-001 (§3.1 手順 1a)
  - tombstone / erase receipt なしで raw object 不在: not_found — KIO-E-STORE-CORRUPT-001
                                        (corruption の疑い — §3.1 手順 5。repair verify-objects を案内)
  - 有効 erase receipt (canonical final event = `erased` — §3.1 手順 5) ありで raw object 不在:
                                        not_found — KIO-E-PURGE-NOT-FOUND-001 (§4.2。`retired` 済み
                                        receipt での欠落は上段の corruption 側 — [10-operations.md §7.5.1](10-operations.md)
                                        の説明範囲限定と整合)
  - scope の .kio に到達できない:        scope_unreachable — scope_path 不達かつ
                                        scope_registry に scope_id 未登録
                                        → KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001
                                        (.kio を再接続 / kio index で registry 再登録すれば回復可能)
  - tool_profile_hash 不一致で chunk 解決不能:
                                        KIO-E-EVIDENCE-RETARGET-REQUIRED-001 (exit 8)
                                        — pointer が現在の chunk へ自動的に切り替わることはない。

補足: receipt-backed shallow tree は `kio open` / `kio view` の Evidence Pointer 本文解決でも
`KIO-E-COMMIT-SHALLOW-001` となる (§3.1 手順 2a)。tree を backup から復元して再実行する。
同じ code は export / diff / `--at <shallow-commit>` 検索 / cursor 再計算にも用いる
([05-runtime.md §2.2](05-runtime.md))。本文を返さない `kio evidence verify` は §4.3 の
`commit_shallow` status semantics を維持する。
```

---

# 4. Dead Evidence Pointer (purge 対応)

「Evidence Pointer の不変性」(§6) と「法務 purge」([05-runtime.md §3](05-runtime.md)) の緊張領域。purge された raw_hash を指す既存 pointer の挙動を以下に固定する (decided — [09-mvp-scope.md §5.2](09-mvp-scope.md))。

## 4.1 Tombstone レスポンス

raw_hash の canonical final event が `purged` の場合 (§3.1 手順 5 の全 marker 正本化 — 個別 tombstone の末尾 event だけで判定しない。= purge 済みだが履歴上は記録。canonical が `retired` なら該当しない)。レスポンス body の `status` は §4.3 の union と同じ語彙 (`tombstoned`) を使う — purge の事実は `purged_*` フィールドが表す:

単発 `open` / `view` / `restore` は `KIO-E-PURGE-TOMBSTONED-001` (exit 4) とし、
以下の body を error context に保持する。

```json
{
  "status": "tombstoned",
  "purged_at": "2026-04-25T12:00:00Z",
  "purged_reason": "legal" | "privacy" | "misingest" | "copyright" | "other",
  "purged_in_commit": "sha256:9f2c...",
  "raw_hash": "sha256:abc...",
  "scope_path": "/Users/foo/Research/.kio"
}
```

「消した事実」は残し、本文・派生 artifact は到達不能にする (= 透明な忘却、[02-philosophy.md](02-philosophy.md))。

## 4.2 NOT-FOUND レスポンス

raw_hash が public tombstone なしで完全削除 (`--erase-tombstone`) されている場合:

```text
error_code: KIO-E-PURGE-NOT-FOUND-001
message: "Evidence target was purged without tombstone record"
context: { raw_hash, scope_path }
```

完全削除は法的要件上必要な場合のみ。デフォルトは tombstone。
`.kio/purge/erase-receipts/` の bounded non-content receipt は public の tombstone 判定・re-ingest barrier には使わない (**fsck の欠落説明 ([10-operations.md §7.5.1](10-operations.md))・手順 5 の not_found 分類 (§3.1 (ii)〜(iii))・手順 6b の欠落説明・resurrection link・同一 marker 自身の lifecycle 管理 (retired / 再 erased の append — [05-runtime.md §3.5](05-runtime.md)) にのみ使用可** — [05-runtime.md §3.5](05-runtime.md)。この列挙が用途の正本)。
receipt は pointer state を tombstoned にせず、re-ingest も阻止しないため、レスポンスは上記
`not_found` である。**ただしこの保証は当該 bytes が store に不在の間のもの** — 同一 bytes が後日
再 ingest され (明示操作に限らず、working tree 残存原本の自動 scan を含む — [05-runtime.md §3.5](05-runtime.md)
の残存警告)、同じ identity の chunk が再生成された場合、既存 pointer は再び alive として
解決される (このとき active tombstone は raw の再 publication と同時に**退役**する — [05-runtime.md §3.5](05-runtime.md)
の resurrection 規則。退役なしには「tombstone 最優先」の解決と両立しない。**purge 前 commit を指す
旧 pointer の解決は retired event の `resurrection_commit` リンク経由** — 手順 6b)。ただし **復活後に解決される
本文は再生成 instance のものであり、purge 前と byte 同一である保証はない** (Markdown content hash
不採用の帰結 — [03-data-model.md §5](03-data-model.md))。また purge 前の commit を指す旧 pointer の
時点検証は、manifest object が purge で失われているため §3.1 手順 6b の降格 (strict = unverifiable) に従う (erase は resurrection barrier ではない設計 — [05-runtime.md §3.5](05-runtime.md)。
「erase 後も永続的に not_found」と読める保証はしない)。

## 4.3 検証 API

AI Agent が過去回答で使った Evidence Pointer の生存確認用:

```bash
kio evidence verify <pointer> [--strict]   # <pointer> の受理形式は §2.3
```

```json
{
  "status": "alive" | "tombstoned" | "not_found" | "scope_unreachable" | "unverifiable" | "registry_duplicate",
  "details": { ... }
}
```

`unverifiable` は `--strict` 時の「時点帰属を検証できない解決」であり、`details.reason` で区別する:
`commit_shallow` (receipt-backed shallow tree の verify-only 観測 — 状況により解消し得る) /
`manifest_missing` (手順 6b — **恒久**)。exit は reason の再試行可能性に従い分岐する — `commit_shallow` のみなら 3 (unshallow で解消し得る)、`manifest_missing` を 1 件でも含めば **4** (恒久 — 再試行で進展しない。[06-cli-spec.md §7](06-cli-spec.md) / [10-operations.md §11.2](10-operations.md) の横断規約「4 = 再試行で進展しない」どおり。details.reason は引き続き全 reason を返す)。
live clone 重複は status `registry_duplicate` (候補一覧つき、exit 3 — §3.1 手順 1)。**sqlite.db が不在・利用不能の場合は status ではなく command-level の
retryable error `KIO-E-INDEX-REBUILDING-001` (exit 3)** — 検査は完了していないため --strict なしでも
0 を返さない (再構築中に既存 sqlite.db が読めても通常応答へ戻らず fail-closed — [05-runtime.md §6](05-runtime.md)。
[06-cli-spec.md §7](06-cli-spec.md)、[05-runtime.md §2.6](05-runtime.md))。
非 strict では従来どおり alive + `commit_shallow: true` で返す。

`--strict`: tombstoned / not_found / scope_unreachable を **error** として扱う (CI / 自動化用)。
exit code: 全 pointer が alive なら 0。tombstoned / not_found があれば **4** (permanent failure)。
**scope_unreachable のみ**の失敗は **3** (retryable — §3.2 のとおり再接続・registry 再登録で回復可能。
permanent の 4 と区別しないと自動化が再試行可能性を判定できない)。
`--strict` なしの verify は検査が完了すれば 0 を返し、生存状態は `status` フィールドで判定する。
exit code の横断規約は [06-cli-spec.md §7](06-cli-spec.md)。

batch verify は canonical CLI の一方の形である。single pointer と `--batch` は Clap の
構造で exactly-one / 相互排他にし、alias・fallback・dual-read は置かない。

```bash
kio evidence verify --batch <pointers.jsonl> [--strict]
```

`<pointers.jsonl>` は strict UTF-8 の JSONL である。logical record は 1..=4096、各行は
pointer JSON object とし、terminal newline は許容するが blank / whitespace-only line は許容しない。
1 logical record の UTF-8 byte 長は delimiter を除き 64 KiB 以下、入力ファイルの exact bytes
（delimiter と terminal newline を含む）は 16 MiB 以下である。行順・重複行は保持する。入力は
regular file・single link (`nlink == 1`) に限り、最終 path entry を nofollow で開く。pre-open /
open / post-open の retained descriptor identity が一致しなければ拒否する。batch 全体で distinct
`scope_id` は 256 以下、認証済み CAS bytes の aggregate は 4 GiB 以下である。

正常な batch output は単一 JSON object だけであり、次の schema を exact とする。`input_sha256` は
delimiter と terminal newline を含む input の exact bytes の SHA-256、`results` は入力と同順で各行を
一度ずつ保持する。`<single exact status object>` は本節の単発 verify response を field 値までそのまま
入れる。`status_counts` は six status (`alive`, `tombstoned`, `not_found`, `scope_unreachable`,
`unverifiable`, `registry_duplicate`) の全 key を常に持つ。

```json
{
  "schema": "kio.evidence.batch-verify",
  "schema_version": 1,
  "input_sha256": "sha256:<lowercase-hex>",
  "strict": false,
  "results": [{"line": 1, "result": <single exact status object>}],
  "summary": {"total": 1, "status_counts": {"alive": 1, "tombstoned": 0, "not_found": 0, "scope_unreachable": 0, "unverifiable": 0, "registry_duplicate": 0}},
  "verified_count": 1
}
```

output publication is all-or-nothing: structural input error, integrity/rebuild/purge/corruption error, or
command-level error publishes no partial `results`. Internal cache は許容するが output authority にはならない。
各行の final status 前に scope authority、registry/index generation、active purge/read barrier を再検査する。
従って batch の duplicate row も cache hit だけで返してはならず、各 row の最終 authority check を通す。

batch 固有の command-level error は、malformed JSONL / invalid UTF-8 / blank line を
`KIO-E-EVIDENCE-BATCH-INPUT-001` (exit 2)、file / line / record / distinct scope limit を
`KIO-E-EVIDENCE-BATCH-LIMIT-001` (exit 2)、aggregate 認証済み CAS byte limit を
`KIO-E-STORE-VERIFIED-BYTES-LIMIT-001` (exit 4)、検査中の scope authority / registry /
index generation drift を `KIO-E-EVIDENCE-BATCH-CHANGED-001` (exit 3) とする。storeまたはprivate owned copyの
unsafe link・pre/open/post identity 不一致は integrity error (exit 4) である。registry snapshotの
command-level preflightには上記§3の分類を適用し、regular sourceのdriftはbounded retry exhaustionで
exit 3とする。いずれも partial output を返さない。

strict batch の exit priority は permanent 4 > retryable 3 > success 0 である。permanent は
`tombstoned` / `not_found` / `manifest_missing`、retryable は `scope_unreachable` /
`registry_duplicate` / `commit_shallow` とする。non-strict は単発 verify semantics を保つ: 検査完了時は
0 だが `registry_duplicate` は 3 である。

release build、warm local SSD、one scope、4096 distinct all-alive rows、network-free の測定目標は
1,000 pointer rows/min 以上である。この性能目標は上記の authority / registry / index / purge barrier を
緩和する根拠にしてはならない。

active purge journal 中の verify は評価を行わず、KIO-E-PURGE 系 retryable (exit 3) を返す
([05-runtime.md §3.5](05-runtime.md) の読取系規約 — marker 耐久化後・削除完了前の窓で
「削除対象が alive」と誤答しないため)。

# 5. Evidence Retarget

```bash
kio evidence retarget <pointer> --at <commit>
```

`--at` は必須で、`<commit>` は同一 scope に存在する full canonical `sha256:<64 lowercase hex>` commit hash に限る。prefix、ref、tag、`HEAD`、`--latest`、default、alias、fallback はない。pointer は単発 verify と同じ strict parser と 64 KiB 上限を使い、argv と `-` stdin のどちらにも同じ上限を適用する。操作は read-only であり、元 pointer、refs、CAS、working tree、source SQLite を変更しない。結果確定前は stdout に何も書かず、`--json` 成功時は単一 JSON object だけを返す。

入力 pointer の optional `path_at_commit` / `heading_path` / `section_id` / byte span は照合 authority ではない。まず単発 canonical verifier と同じ scope resolver、registry duplicate 判定、format-version gate、index/rebuild gate、purge read barrier、point-in-time association 判定で元 pointer を検証する。旧 commit/tree では `(raw_hash, tool_profile_hash)` が一致する entry の UTF-8 byte-order-min path を旧 canonical path とし、その entry の pinned manifest と pointer の chunk CAS から旧 canonical heading / section / span を再導出する。old/target tree が shallow なら推測・HEAD fallback を行わない。

target tree は同じ `raw_hash` を旧 canonical path に持つ exact entry だけを選び、その entry 自身が pin する target `tool_profile_hash` / `gen` / manifest を authority とする。同じ raw が別 path に重複配置されても別 path を先勝ちで選ばない。source SQLite はこの exact target identity の候補 hash を最大 4,096 件列挙するだけであり、各 candidate の chunk CAS、target manifest、normalized-unit CAS、publication/association を独立検証する。旧 canonical `heading_path` との完全一致がちょうど 1 件だけなら成功する。section / span は target chunk から再構築し、照合条件には使わない。semantic fingerprint、embedding/LLM similarity、fuzzy match、confidence は使わない。

0 件は `KIO-E-EVIDENCE-RETARGET-NOT-FOUND-001` / exit 4、複数は `KIO-E-EVIDENCE-RETARGET-AMBIG-001` / exit 4 とする。candidate / history / aggregate 上限超過は `KIO-E-EVIDENCE-RETARGET-LIMIT-001` / exit 4。old/target shallow、active purge、authority drift は既存 retryable 分類 / exit 3、scope unreachable / registry duplicate / index rebuilding / format incompatible も既存分類をそのまま使う。malformed pointer、非 canonical・不存在の `--at` は usage / exit 2、CAS/schema/hash/identity 矛盾と欠落 manifest/chunk は corruption / exit 4 である。失敗を成功 schema の `status` union へ混ぜず、通常の structured error JSON を返す。

production issuer が path/raw/profile/gen/chunk/section/span を持つ新 pointer を再構築し、単発 verify と batch verify が使う single canonical verifier へ再入力して `alive` かつ target commit 帰属を確認する。開始時、candidate 検証後、返却直前に scope binding、target commit/tree、index identity/generation、purge barrier を再検査し、変化は retryable drift として結果を破棄する。

成功 response は次の field だけを exact に持つ。

```json
{"schema":"kio.evidence.retarget","schema_version":1,"status":"retargeted","target_commit":"sha256:<64 lowercase hex>","retargeted_from":{},"new_pointer":{},"match_method":"heading_path_exact"}
```

`retargeted_from` は入力文字列ではなく strict parser が得た canonical Evidence Pointer object、`new_pointer` は production issuer が作る canonical object である。上記 7 field 以外を成功 response に追加しない。

上限は pointer 64 KiB、association 用 commit ancestry 100,000、tree entries は old/target 各 10,000、target candidates は 4,096（SQLite `LIMIT 4097` で超過検出）、manifest は old/target 各 8 MiB、units は manifest ごとに 4,096、normalized-unit / chunk object は各 128 MiB、command aggregate verified CAS bytes は 4 GiB とする。既存 commit/tree object 上限は CAS loader の current bound を共有する。

# 6. 不変性保証 (immutability guarantee)

```
- 既存 Evidence Pointer は Kio によって書き換えられない
- raw_hash / chunk_hash / tool_profile_hash / commit は append-only
- pointer の意味する場所 (= 生成時に解決可能だった raw + chunk) は purge されない限り保持される
- 解決失敗は schema 上区別される (tombstoned / not_found / scope_unreachable / registry_duplicate。verify はさらに unverifiable — §4.3 の 6 値 union が正本)
- auto commit の GC が tree を shallow 化すると、本文を返す `open` / `view` は membership を推測せず
  `KIO-E-COMMIT-SHALLOW-001` で拒否する。tree を backup から復元すれば完全な解決可能性を回復する
  (raw / chunk object 自体は GC で削除されない、[05-runtime.md §2.6](05-runtime.md))
```

これは AI Agent が Kio から取得した Evidence を **長期参照** できる契約となる。

---

# 7. 外部 Agent との相互運用

Kio は Evidence Pointer を **JSON object として AI Agent に返す**。Agent はこれを記憶し、後続の検証・参照・引用に使える。

## 7.1 検索結果に含める形

**検索レスポンス schema の正本は [05-runtime.md §1.7](05-runtime.md) とする**
(本節は従属記述であり、差分が生じた場合は 05 側を正とする — 06 §8 と
[10-operations.md §11.1](10-operations.md) の関係と同型)。
`results[]` の各行は `evidence_pointer` に §2 の schema を**そのまま**埋め込み、
その正規テキスト形を `evidence_uri` として併記する。

> 2026-07-26 の整理: 本節はかつて `preview` field を持つ独自の例を掲げていたが、
> 05 §1.7 が実契約の正本になった際に追随しておらず、実在しない field を示していた。
> 例の二重管理をやめ、正本への参照に置き換えた。

Evidence Pointer との関係で本書が定める点は 1 つある。**`result_type: "image"` の行でも
`evidence_pointer` は画像そのものではなく参照元 chunk を指す** (05 §1.7)。
§2.3 のとおり `kio://<scope_id>/object/image/<hash>` は **object 参照であって
Evidence Pointer ではなく**、commit も tree も `path_at_commit` も持たないため
§3 の解決手順にも §5 の不変性保証にも乗らないからである。画像の実体は同行の
`payload_uri` (object URI) が指し、`kio open` で取得する。

Agent は `evidence_pointer` を保存し、後続のセッションで以下を実行できる:

```
- kio evidence verify <pointer>     生存確認
- kio view <pointer>                全文 view のパス + view-local span (05 §1.7.2)
- kio open <pointer>                原本ファイルを OS で開く
```

## 7.2 引用フォーマット (人間向け)

UI / レポートでは Evidence Pointer を以下に整形して表示することを推奨:

```text
[report.pdf @ sha256:9f2c1a7b04de… > 認証仕様 > API Token > 有効期限]
      ↑                              ↑                      ↑
      path_at_commit                 heading_path           section
```

commit hash の短縮表示は [03-data-model.md §8.1](03-data-model.md) の規則 (先頭 12 hex) に従う。完全な hash は折りたたみ可能。

---

# 8. Evidence Pointer Schema v1

現在受理する Evidence Pointer schema は version `1` だけである。URI の `sv` と inline / batch JSON の `schema_version` は `1` でなければならず、未知 version、未知 field、欠落 field はすべて KIO-E-CONFIG-SCHEMA 系 error (exit 2) で拒否する。legacy reader、migration branch、unknown-field fallback は置かない。
