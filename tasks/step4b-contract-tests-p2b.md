# Step4b 契約テスト仕様書: fsck 拡大 / evidence pointer 解決・verify・retarget (P2-B)

> **Historical record, non-authorizing.** 現行 authority は本文が引用する canonical docs と Rust tests に限る。ID は review provenance のためだけに残し、compatibility、migration、CLI、schema、future work を authorize しない。

> 本書は **実装より先にテストを固定する** ための契約仕様。Rust 実装コードは含まない。
> 正本は `docs/10-operations.md` **§7.5 全体 (§7.5.1 kio repair --verify-objects / §7.5.2 バックアップ運用 /
> §7.5.3 SQLite schema 変更の規約)** と **§3 (Scope Registry)**、`docs/08-evidence-pointer-spec.md` **全体
> (§2〜§8)**、`docs/04-pipeline.md` **§5.7 (Resume と Repair)**。各契約は spec の規範文からのみ期待値を
> 導き、実装が「どう書かれそうか」からは導かない。曖昧・spec 沈黙の点は該当契約の「期待」内に
> `[解釈割れ]` として引用付きで注記し、末尾 §Y に一覧化する (勝手に決めない)。

**担当グループ**: P2-B (fsck / evidence pointer resolve・verify・retarget)。

**対象 U 項目 (当時の gap inventory)**: F 領域 = U39, U40, U41, U42, U43, U44, U45, U46, U47, U144。
G 領域 = U48, U49, U50, U51, U52, U54, U55, U56, U57, U58, U59, U60, U61, U62 (**U53 は判定部を除外** —
canonical final event の 4 分岐アルゴリズムと `purged→tombstoned` 改称そのものは
Phase 1 の **LC8-LC14** が既に契約化済み。本書は参照のみで再契約
しない。U53 のうち本書が引き取るのは evidence verify の **6 値 status union 化**であり、これは U57 の
契約 (§S) に統合済み)。
加えて **Phase 1 引き継ぎ 1 件**: `kio evidence verify` の canonical final event 正本化の共有化
(発注側指示の「LC21 原則」— **LC21** 「検証失敗 marker は
入口非依存で corruption (fsck/resolver/re-purge 統一)」を土台に、本書 §X で**検証失敗に限らない**
canonical dispatch 全体の共有化まで踏み込んで契約化する)。

## 対象外 (他グループ・Phase 3 送り — 混同注意)

- B 領域 (tombstone/erase receipt events[] lifecycle 本体、U13-U21/U35/U36) — Phase 1 で実装済み・
  現行 canonical docs と Rust tests が契約の正本。本書は参照するのみ (§X で明示的に
  参照する箇所のみ引用)
- C 領域 (`kio open` object URI 解決手順・cache — U22-U24) — 別 P2 グループ。本書 §M (U50) は
  pointer **schema** としての object URI type 制約 (発行・受理の型そのもの) のみを扱い、`kio open` の
  cache 配置・EEXIST 処理・3 点起動前検査などの実装機構には立ち入らない
- D 領域 (restore の退避・隔離・no-replace publish — U25-U27) — 別 P2 グループ
- E 領域の残り (purge 削除範囲・staging・ログ scrub・working tree 警告・二重 purge — U28-U34/U37/U38) —
  別 P2 グループ。E 領域のうち journal/epoch 機構本体 (U35/U36) は Phase 1 実装済みで
  現行 canonical docs と Rust tests が正本（LC39-LC57 は provenance）
- H 領域 (検索 gate/cursor/時点条件/multi-scope — U63 以降) — 別 P2 グループ。本書 §H (U46) は
  registry の live 重複**検出**契約のみを扱い、横断検索の `excluded_scopes` 統合・partial 表示は H 側の
  管轄 (該当箇所で明示的に境界を注記する)
- A 領域残り (U1-U4/U11/U12)・I/J 領域残り・K/L 領域 — Phase 3
- `kio evidence retarget` の実装そのもの (U59/U60) は 08§5 が明記するとおり **Phase 4+** — 本書は
  「実装されたときに満たすべき契約」を先行して固定するのみで、実装着手を要求しない

## 実装対象ファイルの見込み (現状把握の記録 — 実装方針を指図するものではない)

- `crates/kio-cli/src/verify_objects.rs` — `verify_objects_with_limits` の検証対象拡大 (embedding/
  manifest/toollock 追加、L307-673 の inventory ループ群に新設種別を追加)、`verify_pointer_for_cli`
  (L96-196) の canonical dispatch 共有化 (現状は `read_tombstone`/`PurgeState::barrier_blocks` の
  単一 marker 判定 — §X 参照)、`commit_roots` (L1348-1501) への names.jsonl 検証追加、
  `--prune-orphans` 新設 (`run_evidence`/`verify_objects` 双方に隣接する新規モード)
- `crates/kio-cli/src/main.rs` — `parse_repair_args`/`RepairMode` (L1004-1067) への `PruneOrphans`/
  `RegistryPrune` 追加、`resolve_scope_id_in_registry` (L6368-6411) の fail-closed 化、
  `resolve_pointer_for_cli` (L6170-6356) への手順 6a/6b 追加、`parse_object_uri` (L7281-7315) の
  `VALID_TYPES` 縮小、`run_evidence`/`verify_pointer_for_cli` 側の `--strict` reason 別 exit 分岐
- `crates/kio-core/src/dag.rs` — `NormalizeRef` (L15-20) への `manifest_hash` field 追加 (U40 の前提。
  現状は `tool_profile_hash`/`gen` のみ)
- `crates/kio-core/src/cas.rs` — `ObjectKind` (raw/tree/commit のみ)・`ContentObjectKind`
  (prepared/image のみ) への manifest/toollock/embedding 種別追加 (`objects/manifests/`・
  `objects/toollocks/` は現状 grep 0 件)
- `crates/kio-index/src/fts.rs` — current schema は `chunk_publications` relation、pair-only の
  `chunk_config_generations`、`index_metadata` を持つ
- `crates/kio-index/src/registry.rs` — 直接の変更対象ではない (upsert/lookup は現状のまま)。fail-closed
  判定は main.rs 側の呼び出し元が担う
- `crates/kio-search/src/evidence.rs` — `EvidencePointer`/`ValidatedEvidencePointer` は現状すでに
  schema_version の MAJOR-only 数値・未知フィールド許容・URI 大文字小文字保存を満たす (§K/§L で規範
  確認の regression-lock 契約とする)

## 表記

`### PB<連番> <契約タイトル> [P レベル]` の後に `正本` (§ + 行番号 + 該当規範の 1 文引用) / `前提` /
`操作` / `期待` を置く。P0 = このロットの完了条件、P1 = 推奨 (周辺・堅牢性・regression-lock)、
P2 = 参考 (Phase 4+ 依存・文書のみ)。「**現行実装との既知の不整合**」は 2026-07-22 時点のコード読解
(Step 4b Phase 1a/1b/1c 適用後、commit `19bf78d` 時点) に基づく脚注であり、契約そのものではない。

---

## 0. ID 体系と優先度

| 接頭辞範囲 | 対象契約領域 | U 項目 | 主根拠 |
| --- | --- | --- | --- |
| PB01-03 (§A) | fsck 検証対象拡大 (embedding/manifest/toollock) | U39 | 10 §7.5.1 L489 |
| PB04-06 (§B) | manifest object 再 hash 検証 | U40 | 10 §7.5.1 L493-504 |
| PB07-09 (§C) | tag canonical ref + names.jsonl 全行検証 | U41 | 10 §7.5.1 L505-509 |
| PB10-11 (§D) | done object 復旧禁止・legacy 警告 exit 非影響 | U42 | 10 §7.5.1 L515-529 |
| PB12-17 (§E) | `--prune-orphans` 新設 | U43 | 10 §7.5.1 L586-626 |
| PB18 (§F) | embeddings query_cache rebuild 例外 | U44 | 10 §7.5.1 L511 |
| PB19-20 (§G) | SQLite schema 変更規約 | U45 | 10 §7.5.3 全体 |
| PB21-26 (§H) | registry live 重複 fail-closed + `--registry-prune` | U46 | 10 §3 L284-299 |
| PB27 (§I) | バックアップ最低保全集合 | U47 | 10 §7.5.2 L641-644 |
| PB28-31 (§J) | rebuild-db index_metadata 初期化 + introduction 再導出 | U144 | 04 §5.7 |
| PB32-33 (§K) | schema_version wire 表現統一 | U48 | 08 §2.1 L56, §2.3 L100-104 |
| PB34-36 (§L) | 表示フィールド canonical 優先・URI opaque | U49 | 08 §2.2 L73-79 |
| PB37-38 (§M) | object URI type=image 限定 | U50 | 08 §2.3 L107-117 |
| PB39-41 (§N) | receipt-backed shallow の open/view fail-closed と verify-only 観測 | U51 | 08 §3.1 手順 2a / §4.3 |
| PB42-44 (§O) | 手順 4: 多重 entry 決定的選択 | U52 | 08 §3.1 L172-178 |
| PB45-47 (§P) | 手順 6a: 時点帰属検証 | U54 | 08 §3.1 L207-221 |
| PB48-50 (§Q) | 手順 6b: manifest 欠落降格 | U55 | 08 §3.1 L222-253 |
| PB51-52 (§R) | 手順 8: defense-in-depth | U56 | 08 §3.1 L254-266 |
| PB53-57 (§S) | evidence verify status 6 値 union 化 | U57, U53 (verify 部分) | 08 §4.3 |
| PB58 (§T) | purge journal 進行中の evidence verify 拒否 | U58 | 08 §4.3 L387-389 |
| PB59-61 (§U) | retarget fail-closed 強化・match_method | U59, U60 | 08 §5 |
| PB62 (§V) | evidence verify `--batch` | U61 | 08 §4.3 L391 |
| PB63 (§W) | `path_at_commit` legacy tree 例外 | U62 | 08 §2 L50 |
| PB64-68 (§X) | evidence verify の canonical validator 統一 (Phase 1 引き継ぎ) | LC21 原則 | 05 §3.5, 08 §3.1 手順 5 |

**優先度集計は末尾「§集計」節**。

---

## A. fsck 検証対象拡大 (U39)

> 正本: 10 §7.5.1 L489『`objects/` 配下の全 CAS object (raw / prepared / image / chunk / embedding /
> manifest / toollock / tree / commit) を 03 §8.1 の per-type algorithm で検証し (embedding は
> vector 長・有限値・vector digest も — 03 §8.1)』

### PB01 embedding CAS object の vector 検証 (長さ・有限値・digest、パラメタ化) [P0]
- 正本: 10 §7.5.1 L489 (上記引用)。
- 前提: `objects/embeddings/` に embedding CAS object が存在する想定 (U40 と同様、`embedding`
  という CAS 種別自体が現状 `ObjectKind`(`cas.rs` L144-150: Raw/Tree/Commit のみ)・
  `ContentObjectKind`(`cas.rs` L22-25: Prepared/Image のみ) いずれにも無い — 本契約はこの型が
  新設されることを前提とする)。dimensions 宣言値と実 vector 長が (a) 一致、(b) 不一致。vector 要素に
  (c) 全て有限、(d) `NaN` 混入、(e) `Infinity` 混入。object 本体の digest が (f) 一致、(g) 不一致。
- 操作: `kio repair --verify-objects` を実行する。
- 期待: (a)(c)(f) は成功として検証通過。(b)(d)(e)(g) はいずれも finding (`embedding_corrupt` 相当)
  として報告される。**現行実装との既知の不整合**: `verify_objects.rs` に `"embed"`/`"dimension"`/
  `"finite"` の grep が 0 件であり、embedding の検証対象化そのものが未着手。

### PB02 manifest / toollock を検証対象クロージャに追加 (対象種別の closure 化) [P0]
- 正本: 10 §7.5.1 L489 (raw/prepared/image/chunk/embedding/**manifest**/**toollock**/tree/commit の
  9 種別列挙)。
- 前提: `.kio/objects/manifests/`・`.kio/objects/toollocks/` に CAS object が存在する (03 §2 の
  レイアウト)。
- 操作: `kio repair --verify-objects` を実行し、検証対象種別の一覧を検査する。
- 期待: manifest object (詳細は §B、PB04-06) と toollock object (canonical JCS bytes の content hash
  検証 — 03 §5.2) がいずれも fsck の検証対象種別に含まれる。**現行実装との既知の不整合**:
  `objects/manifests`・`objects/toollocks` は crates/ 全体で grep 0 件 (本コマンドで確認済み) —
  検証対象化の前提となる CAS 種別自体が存在しない。

### PB03 [regression-lock] chunk の exact text/text_hash/normalized span 照合は既に正しい [P1]
- 正本: 10 §7.5.1 L490-492『chunk は object bytes の content hash ではなく semantic identity hash と
  fan-out key、さらに exact `text` / `text_hash` / normalized span を照合する』
- 前提: 現行実装 (`verify_objects.rs` L896-947) は chunk ごとに normalized instance の対応 unit を
  引き当て、`unit.markdown.get(byte_start..byte_end)` の exact スパンが `chunk.text` と一致し、かつ
  `hash_bytes(exact) == chunk.text_hash` であることを検証している。
- 操作: (a) span がずれた chunk (`chunk_span_mismatch`) を用意する。(b) 正当な chunk を用意する。
- 期待: (a) は finding、(b) は finding なし — 既存実装がこの規範を既に満たすことを固定する
  regression-lock (新規実装は不要、既存挙動の維持を保証する)。

---

## B. manifest object の再 hash 検証 (U40)

> 正本: 10 §7.5.1 L493-497『**manifest object (objects/manifests/) は content-addressed であり再 hash
> 検証の対象**: 各 tree entry の `normalize.manifest_hash` が実在する manifest object を指し、**かつ
> 当該 manifest の (raw_hash, tool_profile_hash, gen) が entry 側と一致する**こと (hash が正しいだけの
> 別 instance manifest への誤配線検出)』

### PB04 tree entry の normalize.manifest_hash 追加と CAS 再 hash 検証 (パラメタ化) [P0]
- 正本: 10 §7.5.1 L493-497 (上記引用)。
- 前提: `NormalizeRef` (`kio-core/src/dag.rs` L15-20) が `manifest_hash` field を持つ (現状は
  `tool_profile_hash`/`gen` のみ — 本契約の前提として追加される)。tree entry に (a) 実在する
  manifest object を指す `manifest_hash`、(b) 存在しない hash を指す `manifest_hash`、(c) 実在するが
  別 instance ((raw_hash, tool_profile_hash, gen) が entry 側と不一致) の manifest を指す
  `manifest_hash`。
- 操作: `kio repair --verify-objects` を実行する。
- 期待: (a) は検証通過。(b) は manifest 欠落 finding (§B の説明範囲規則 — PB05 — に該当しない限り
  corruption)。(c) は「hash が正しいだけの別 instance manifest への誤配線」として corruption
  finding。**現行実装との既知の不整合**: `manifest_hash` field 自体が `dag.rs` に存在しないため、
  このクロスチェックの前提スキーマが無い。

### PB05 purge が説明する missing manifest の除外スコープ (in_commit 以前限定) [P0]
- 正本: 10 §7.5.1 L497-500『tombstone / erase receipt が説明する purge 済み raw の entry を除く —
  下記 dead terminal 規則。purge は manifest object を削除するが tree は書き換えないため、この例外
  なしには正規 purge 直後の store が必ず corruption になる』/ L534-536 の説明範囲限定 (「当該 purge
  event の `in_commit` **以前**の commit が参照する closure に限る」) が manifest 検証にも同一に
  適用される。
- 前提: raw_hash `X` が canonical final event = `purged` (in_commit=`Cp`) で説明される。(a) `Cp` 以前の
  commit の tree entry の manifest が欠落。(b) `Cp` より後の (= retire 後に再作成・再公開された)
  commit の tree entry の manifest が欠落。
- 操作: `kio repair verify-objects` を実行する。
- 期待: (a) の厳密に purge が説明する古い reachable commit の normalized closure 欠落は、
  expected unavailable history とし、`purge_explained_missing_history_count` に normalize 参照ごとに
  算入する (object / unit 数ではない)。他に finding / incomplete が無ければ `status="ok"` だが、
  count > 0 では `external_pointers_may_be_affected=true` を維持する。全履歴の復元可能性は保証しない。
  (b) は「古い退役 event が新規破損を隠さない」ため corruption と判定する
  (`manifest_corrupt` 相当の finding)。この判定は §D(PB05 自身)・fsck 側の raw 欠落説明スコープ
  (LC17/LC35-38 と同一原則) を manifest object に対しても適用する
  ことを要求する — raw 側だけ範囲限定して manifest 側は無条件除外、という非対称実装は契約違反。

- 追加条件: 有効な canonical purge / lifecycle evidence と、`manifest_hash` を含む exact old
  `NormalizeRef` を要求する。cutoff は検証済み `purged` / `erased` event の `in_commit` 以前であり、
  resurrection 時点まで広げない。同じ exact closure が HEAD / current、または purge 後の commit に
  現れれば例外を拒否する。resurrection commit / 新 generation の欠落にも適用しない。古い historical
  tag root であることだけでは除外しない。manifest の全構造・残存全 body を検査し、一つ目の missing
  unit の後にある corrupt unit も finding とする。genuine missing、corrupt manifest / body、未検証・
  不正 marker は除外しない。通常 verify / prune / replica / image authority で共有する分類を使い、
  prune 専用 bypass は認めない。purge → re-ingest 後の検索は現在の有効な closure から回復し、
  削除済みの古い closure は image ownership を与えない。公開経路の regression と shallow partial
  ケースは 2026-09-27 の macOS CLI contract suite（71件）で通過した。
  Windows/Linux の候補全体の runtime 受入と新しい差分のセキュリティ確認は別の未完了 gate とする。

### PB06 HEAD tree entry の作業コピー manifest.json canonical JCS hash 一致検査 (未 finalize と corruption の分離) [P0]
- 正本: 10 §7.5.1 L501-504『HEAD tree の entry については作業コピー manifest.json の canonical JCS
  hash が一致することも検査する (不一致 = 破損ではなく「未 finalize の進行状態」として incomplete
  (exit 3) — manifest finalize と次回 snapshot の間のクラッシュ窓で正常に生じる。... corruption と
  するのは manifest object 自体の再 hash 不一致のみ)』
- 前提: (a) HEAD commit の tree entry が指す manifest object の内容と、対応する
  `normalized_units/.../manifest.json` (作業コピー) の canonical JCS hash が一致。(b) 不一致
  (finalize と次回 snapshot 間のクラッシュ窓を模したもの)。(c) manifest object 自体の再 hash が
  そもそも不一致 (PB04(c) の corrupt CAS bytes ケース)。
- 操作: `kio repair --verify-objects` を実行する。
- 期待: (a) 検証通過。(b) は exit 3 の incomplete として報告され、corruption 件数には算入しない。
  (c) は corruption (exit 4 系 finding) として (b) と明確に区別される。非 HEAD commit の tree entry
  にはこの作業コピー照合を適用しない (作業コピーは常に最新世代を指すため、過去 commit の manifest と
  比較する意味がない)。

---

## C. tag canonical ref + names.jsonl 全行検証 (U41)

> 正本: 10 §7.5.1 L505-509『canonical tag ref (`refs/tags-v1/tag-*`) と `names.jsonl` (論理名の
> truth) は**全行**を検査する: 各行の schema、`digest64` ↔ `logical_name` の対応 (digest 再計算)、
> torn tail (最終の不完全行のみ切詰め — 途中の malformed 行は corruption)、各 canonical ref ↔
> 最終有効行の対応 (03 §2 と同一規則)。対応行の無い canonical ref は corruption (ref の無い names
> 行は tag 削除後の残存として正常)』

### PB07 names.jsonl 各行の schema 検証と digest64 再計算一致 [P0]
- 正本: 10 §7.5.1 L505-507 (上記引用の前半)。
- 前提: `names.jsonl` (03 §2 の論理名 truth ログ) に (a) schema 正当かつ `digest64` が
  `portable_tag_leaf(logical_name)` の再計算値と一致する行、(b) schema は正当だが digest64 が
  再計算値と不一致な行、(c) 必須 field を欠く schema 不正な行。
- 操作: `kio repair --verify-objects` を実行する。
- 期待: (a) は検証通過。(b)(c) はいずれも corruption finding。**現行実装との既知の不整合**:
  `names.jsonl`/`names_jsonl` は crates/ 全体で grep 0 件 — 現行の tag 検証 (`verify_objects.rs`
  L1348-1501 `commit_roots`) は `refs/tags-v1/tag-<digest64>` (canonical) と `refs/tags/<logical_name>`
  (legacy、`portable_tag_leaf` で digest 再計算し一致検査) の**二重書き込み方式**による整合検査のみで
  行っており、`names.jsonl` という論理名 append log 自体が存在しない (§Y-1 参照 — この構造差が
  実質的に同等の保証を与えるかは解釈が割れうる)。

### PB08 torn tail のみ許容・途中 malformed 行は corruption [P0]
- 正本: 10 §7.5.1 L507-508『torn tail (最終の不完全行のみ切詰め — 途中の malformed 行は corruption)』
- 前提: `names.jsonl` の (a) 最終行のみが途中で切れている (crash-safe append の想定される残骸)。
  (b) 途中の行 (最終行ではない) が malformed。
- 操作: `kio repair --verify-objects` を実行する。
- 期待: (a) は最終行を切り詰めて正常読み取りとして扱う (corruption としない)。(b) は corruption
  finding として報告する — 「末尾のみ寛容、途中は不寛容」という非対称を実装が正しく区別すること
  (両方寛容にする実装、両方不寛容にする実装のいずれも契約違反)。

### PB09 canonical ref ↔ names 行対応の非対称判定 (ref 無し names 行は正常) [P0]
- 正本: 10 §7.5.1 L508-509『対応行の無い canonical ref は corruption (ref の無い names 行は tag
  削除後の残存として正常)』
- 前提: (a) `refs/tags-v1/tag-<digest64>` が存在するが、`names.jsonl` に対応する最終有効行が無い。
  (b) `names.jsonl` に行はあるが、対応する `refs/tags-v1/tag-<digest64>` が存在しない (tag 削除後)。
- 操作: `kio repair --verify-objects` を実行する。
- 期待: (a) は corruption finding。(b) は finding なし (tag 削除後の残存として正常 — names.jsonl は
  append-only で削除時に行を落とさない設計であることを示唆する)。この非対称を実装が両方向とも
  corruption として扱う、または両方向とも許容してしまうことは契約違反。

---

## D. done object 復旧禁止・legacy 警告 exit 非影響 (U42)

### PB10 normalized unit done object 欠落の same-gen 再生成禁止 [P0]
- 正本: 10 §7.5.1 L521-523『(normalized unit の done object 欠落も同様 — same-gen 再生成は行わない
  (unit object は immutable であり、非決定的な再生成は過去 commit の内容差し替えになる)。復元は
  backup restore、または明示の新 gen (kio reindex --force) で行う)』
- 前提: HEAD tree entry が指す normalized instance (raw_hash, tool_profile_hash, gen=N) の unit
  done object が欠落し working tree に原本が残っていない (recover_raw が適用できないケース)。
- 操作: `kio repair --verify-objects` を実行する。
- 期待: 同一 gen=N での自動再生成 (再 Markdownize) は一切行われない — missing として finding のみ
  報告する (`normalized_corrupt` 相当)。復旧手段として backup restore、または `kio reindex --force`
  による新 gen 発行のいずれかのみを案内する。**現行実装との既知の不整合**: `manifest_hash` 不在
  (PB04) につき「done 宣言 object」概念自体が明示的スキーマとして無く、この禁止規則を明文で強制する
  ロジックも無い。

### PB11 legacy 警告 (path/reason) の件数分離と exit code 非影響 [P1]
- 正本: 10 §7.5.1 L527-528『exit code: 破損 0 件 または 全件復元 = 0 / missing 残あり = 3 (legacy
  警告 (path / reason) は exit に影響しない — 破損とは別に種別ごとの件数を表示する)』
- 前提: (a) legacy 警告 (path 形式の legacy・reason enum 外の legacy) が複数件あるが corruption は
  0 件。(b) legacy 警告 0 件で corruption も 0 件。
- 操作: `kio repair --verify-objects` を実行する。
- 期待: (a)(b) いずれも exit code は 0 (legacy 警告の有無・件数は exit を変えない)。legacy 警告は
  corruption カウンタとは別の種別ごとの件数として出力に含まれる。この規則は
  **LC7** が tombstone/receipt の reason legacy 警告について
  既に固定済みであり、本契約はそれを normalized unit の path/reason legacy 警告一般に拡張する
  regression-lock として位置づける (再定義ではなく適用範囲の確認)。

---

## E. `--prune-orphans` 新設 (U43)

> 正本: 10 §7.5.1 L586-614『`kio repair --verify-objects --prune-orphans` は、どの manifest からも
> 参照されない orphan prepared / image ... と descriptor の無い staging root・path と不整合な staging
> root ... terminal 化済み ... task にのみ対応する staging root ... を列挙し、locked repair として
> 削除する』

### PB12 CLI フラグ追加と `--rebuild-db`/`--verify-objects [--prune-orphans]`/`--registry-prune` の exactly-one 構文 [P0]
- 正本: 10-operations.md §7.5.1 導入部の `kio repair --verify-objects --prune-orphans` 構文、および
  U46 統合要約の『`kio repair` は `(--rebuild-db [--online|--offline] | --verify-objects
  [--prune-orphans] | --registry-prune)` の exactly-one 必須構文に拡張する』（当時の U46 統合要約、
  出典 gap-10-03 G11 等)。
- 前提: `kio repair` を (a) `--verify-objects --prune-orphans`、(b) `--prune-orphans` 単独 (
  `--verify-objects` を伴わない)、(c) `--rebuild-db --prune-orphans` (併用不可の組み合わせ) で実行。
- 操作: 各 CLI 呼び出しを行う。
- 期待: (a) は受理される。(b) は `--prune-orphans` は `--verify-objects` の修飾フラグでありそれ単独
  では無効、として exit 2 (invalid usage)。(c) も同様に exit 2。**現行実装との既知の不整合**:
  `parse_repair_args` (`main.rs` L1004-1067) は `--rebuild-db`/`--verify-objects` の 2 択のみを受理し、
  `--prune-orphans`/`--registry-prune` は grep 0 件 (unknown flag として拒否される)。

### PB13 orphan prepared/image の検出条件と削除 (manifest 非参照) [P0]
- 正本: 10 §7.5.1 L586-588『どの manifest からも参照されない orphan prepared / image (公開前 crash
  の残骸)』
- 前提: (a) いずれの normalized instance の manifest からも参照されない prepared object。(b) 何らかの
  live manifest から参照される prepared object。(a)(b) と同様に image object も用意する。
- 操作: `kio repair --verify-objects --prune-orphans` (確認プロンプトを承認) を実行する。
- 期待: (a) は削除対象に含まれ locked repair として物理削除される。(b) は削除されない (live 参照が
  ある限り保持)。この live 参照判定は purge closure の共有派生判定 (U30、02-philosophy §6.1) と同一
  規則を使う。

### PB14 staging root と normalized private stage の分類 [P0]
- 正本: [10-operations.md §7.5.1](../docs/10-operations.md#751-kio-repair-verify-objects-fsck-相当)。
- 前提: (a) descriptor 無し、(b) 妥当な同一 scope の descriptor と path が不整合、(c) 対応 task が
  done / failed permanent / abandoned / settled partial、(d) pending / running / paused / failed
  retryable / retryable failed unit の残る partial、(e) malformed / foreign descriptor を用意する。
- 操作: `kio repair verify-objects --prune-orphans` の対象表示を確認して承認する。
- 期待: 全 scope blocker が無ければ (a)(b)(c) は候補。(d) は scope 全体を拒否し、(e) や hardlink /
  symlink / reparse point 等の unsafe な残骸も計画を拒否する。「descriptor 無し / path 不整合」を
  malformed / foreign descriptor の削除許可に読み替えない。
- 追加契約: publication の所有下にない abandoned normalized `.staged-<ULID>` は、全 blocker が
  無ければ候補。妥当な staged manifest の prepared / image 参照は今回の計画で live として保護する。
  stage 削除後に孤立する CAS は、次回の新しい preview / proof / 確認でのみ回収する。

### PB15 scope 全体の blocker と確認済み計画の再検査 [P0]
- 正本: 10 §7.5.1 の拒否条件・確認から削除まで・現行の計画上限。
- 前提: 他の削除可能な orphan と共に、各条件を単独で用意する: (a) state 0/1 `batch_requests`
  (`request_kind` の各種および NULL `intent_token` を含む)、(b) non-terminal task (paused /
  failed retryable / retryable partial を含む)、(c) 未 finalize projection / immutable manifest 不一致、
  (d) active publication / pending atomic / active purge journal。
- 操作: `kio repair verify-objects --prune-orphans` を実行する。
- 期待: いずれも prune 全体を exit 3 で拒否し、一件も削除しない。blocker 種別・対象・次操作を示す。
  公開・整合検証済み manifest の retryable failed unit だけでは publication blocker にしないが、
  その prepared / image 参照は保護する。state 2/3 request と settled partial は blocker ではない。
- 確認後の追加条件: apply 直前に blocker / live 参照 / 対象実体を変更する。全 blocker・現在の proof・
  現在も eligible として選択する全 pin を最初の削除前に再検査し、元の表示対象で現在も eligible な同一実体だけを削除する。
  新しい orphan や同名の置換物を対象に追加しない。検査・削除エラーを無視して成功を返さない。
- 上限: core RemovalBudget の 1,024 held handle / pin、深さ 32、捕捉物理 file size 合計 1 GiB、
  proof walk の 1,000,000 entries / 計上 verification byte 合計 1 GiB 超過では明示拒否し、無言で
  候補を切り詰めない。core の複数 pass の hash 再読込、shared CAS loader の検証 I/O は累積で
  1 GiB 以下とは限らない。proof は直接読む descriptor / projection / immutable manifest と、正常に
  load した unit / commit / tree の canonical-size を計上する。個別 object / instance、失敗時、
  検証 pass は別の上限を持ち、shallow receipt helper は上限付き 2 pass とする。

### PB16 特定不能退出経路の両立と crash quarantine 回復 [P1]
- 正本: 10 §7.5.1 の特定不能退出経路 (1)(2) と crash quarantine 回復。
- 前提: 妥当で path と整合する descriptor の対応 task record が失われている (04 §1)。
- 操作: `kio repair verify-objects --prune-orphans` の対象表示を確認して承認する。
- 期待: 全 scope blocker の不在を必須とし、次の**両方**の退出経路を検査する。
  (1) 同一 (raw_hash, tool_profile_hash) に少なくとも一世代が存在し、存在する全 gen の全 unit が
  terminal (done / failed permanent)、同 key の state 0/1 request が無い場合は terminal 残骸として候補。
  ゼロ世代を空集合の全称条件で terminal としない。
  (2) (1) 以外でも、全 blocker の不在を lock 下で確認し、明示確認した locked repair なら候補。
  task 記録の欠落だけで永久拒否せず、ledger / publication / unsafe 検査は省略しない。
- crash 回復: `.kio-cas-remove-…` / `.kio-prune-directory-…` 残存は新しい preview と現在の proof、
  新しい確認済み pin を必要とする。名前だけを権限にせず、canonical 名との共存や不正状態を拒否する。
  複数対象の途中失敗には一括 rollback を仮定せず、残存物を再計画する。

### PB17 purge 済み raw の open cache 残骸回収 (raw/image 型分離、C 領域との境界注記) [P1]
- 正本: 10 §7.5.1 L616-626『`--prune-orphans` は、当該 scope で canonical final event が `purged`
  **または `erased`** である各 raw_hash について `~/.cache/kio/open/<raw_hash digest64>/` の残存も
  検査し、存在すれば同じ locked repair の削除対象に含める ... **image cache も同様に回収する**』
- 前提: canonical final event が `purged`/`erased` の raw_hash に対応する `~/.cache/kio/open/<digest64>/`
  が残存する (open 手順の publish 後・起動直前検査前の crash 窓を模したもの)。当該 scope のどの live
  manifest からも参照されない image の `~/.cache/kio/open/image/<digest64>/` も同様に残存する。
- 操作: `kio repair verify-objects --prune-orphans` を実行する。
- 期待: いずれの残存 cache dir も削除対象に含まれ、同じ locked repair で冪等に削除される。**境界
  注記**: cache の型分離 (`open/image/<digest64>/` への raw/image 分離自体) は C 領域 (U22-U24) の
  管轄であり本書は再契約しない — 本契約は `--prune-orphans` という**本書 F 領域の CLI フラグ**が
  この削除を trigger することのみを固定する。

---

## F. embeddings query_cache 行の SQLite rebuild 時例外 (U44)

### PB18 [regression-lock] target_type='query_cache' 行は rebuild 時に自然消滅する [P1]
- 正本: 10 §7.5.1 L510-511『SQLite index は検証対象外 (破損時は `--rebuild-db` で再構築可能なため。
  embeddings の `target_type='query_cache'` 行のみ復元されず破棄 — 影響は cursor 拒否 04§4.3)』
- 前提: 現行実装 `snapshot_chunk_embeddings` (`kio-index/src/embedding_store.rs` L308-324 相当) は
  `embeddings e JOIN chunks c ON e.target_type = 'chunk' AND c.text_hash = e.target_id` という INNER
  JOIN で rebuild 前後にわたり保持する行を選別している。
- 操作: (a) `target_type='chunk'` の embedding 行を用意して `kio repair --rebuild-db` を実行する。
  (b) `target_type='query_cache'` の embedding 行を用意して同様に実行する。
- 期待: (a) は rebuild 後も保持される。(b) は rebuild 後に消失する (INNER JOIN の条件で
  `target_type != 'chunk'` の行が構造的に除外されるため) — 新規実装は不要、既存の JOIN 条件が
  この規範を既に満たすことを固定する regression-lock。

---

## G. SQLite schema 変更規約 (rebuild vs in-place migration、U45)

### PB19 schema 変更の既定経路は rebuild、cost-ledger.sqlite のみ例外 [P1]
- 正本: 10 §7.5.3 L687-691『schema 変更のデフォルト経路は **migration を書かず再構築する** こと
  (sqlite.db は `kio repair --rebuild-db`、registry は各 `.kio` の rescan)。**`cost-ledger.sqlite` は
  このデフォルトの対象外**』
- 前提: sqlite.db (検索加速層) と scope-registry.sqlite (registry) のいずれかに新しい列/表を追加する
  必要が生じた、という仮想シナリオ。
- 操作: 当該変更の実装方針を検証する (コードレビュー水準の構造チェック — 新規 `ALTER TABLE` ベースの
  in-place migration 関数が sqlite.db/registry 用に追加されていないことを確認する)。
- 期待: sqlite.db は `kio repair --rebuild-db` (既存の `rebuild_sqlite_index`) が唯一の schema 更新
  経路であり、registry は起動時の `CREATE TABLE IF NOT EXISTS` (`registry.rs` L76-86) のみで
  migration 関数を持たない。cost-ledger.sqlite (Phase 1 実装済み) だけが in-place migration の対象
  である。**[解釈割れ]**: 本契約は「現在そうなっている」ことの確認であり、将来の変更が既定経路を
  破らないことまでは自動テストで保証できない (§Y-2 参照)。

### PB20 incompatible SQLite schema は mutation なしで fail-closed [P0]
- 正本: 10 §7.5.3 L710-718『例外として in-place migration を書いてよいのは次の場合のみ: 1. append-only
  データの保全が必要な場合 例: ... 旧 `chunks.chunking_config_hash` 列 → `chunk_config_generations`
  relation への分離 (Step 3) は in-place migration とした (実装済みの先例) 2. 起動のたびに全再構築
  するのが非現実的な大規模 store』
- 前提: source index は current schema と一致しない bytes を reader として受け入れない。
- 操作: non-current table/column を持つ sqlite.db を開き、通常の command が mutation 前に
  `KIO-E-STORE-CONSTRAINT-001` で停止することを確認する。
- 期待: current schema は rebuild-only である。`ALTER TABLE` migration、旧列 reader、dual-read は
  存在しない。復旧は明示的な `kio repair --rebuild-db` だけである。

---

## H. registry live 重複の fail-closed 処理 + `kio repair --registry-prune` (U46)

> 正本: 10 §3 L284-287『同一 scope_id の複数 live path は clone 併存であり、**fail-closed で扱う**:
> global search は当該 scope_id を skip して `excluded_scopes` に `KIO-E-REGISTRY-DUP-001` の理由付きで
> 記録し、pointer 解決は候補一覧 error とする』

### PB21 live 重複は last_seen_at の差に関わらず fail-closed (自動選択の廃止) [P0]
- 正本: 10 §3 L284-285 (上記引用)、08 §3.1 手順 1b L152-155『同一 scope_id が複数 **live** 登録されて
  いる場合は選択しない — `KIO-E-REGISTRY-DUP-001` の候補一覧 error で fail-closed とし、dedupe を
  要求する ... purge 状態の異なる clone へ黙って解決すると scope 単位 purge の判定を取り違えるため』
- 前提: 同一 scope_id で 2 つの live `.kio` 登録が存在し、`last_seen_at` が (a) 完全一致 (タイ)、
  (b) 異なる (一方が明確に新しい)。
- 操作: `kio evidence verify <pointer scope_id=当該>` を実行する。
- 期待: (a)(b) いずれも候補一覧 error で fail-closed とする (どちらを解決対象にするか自動選択しない)。
  **現行実装との既知の不整合**: `resolve_scope_id_in_registry` (`main.rs` L6368-6411) は `last_seen_at`
  が**タイの場合のみ** `scope_ambiguous_error` (`KIO-E-EVIDENCE-SCOPE-AMBIGUOUS-001`) を返し、タイで
  なければ (b) のケースで**依然として最新優先の自動選択**を行う (L6397-6410 — 「旧仕様の
  last_seen_at 最新を優先する自動選択」が temporal-tie 判定の陰でまだ生きている)。これは (b) のケース
  で本契約に反する。

### PB22 KIO-E-REGISTRY-DUP-001 エラーコード + REGISTRY namespace 新設 [P0]
- 正本: 10 §3 L285『`KIO-E-REGISTRY-DUP-001` の理由付きで記録』/ U46 統合要約『エラー namespace に
  REGISTRY domain を新設する』
- 前提: PB21 (a)(b) いずれかの live 重複状態。
- 操作: `kio evidence verify`/`kio open`/`kio view`/`kio restore` のいずれかで当該 scope_id を解決する。
- 期待: 返るエラーの `error_code` は `KIO-E-REGISTRY-DUP-001` (namespace `REGISTRY`) である。
  **現行実装との既知の不整合**: `KIO-E-REGISTRY-DUP-001` は crates/ 全体で grep 0 件 (本コマンドで
  確認済み) — 現行は `KIO-E-EVIDENCE-SCOPE-AMBIGUOUS-001` (namespace `EVIDENCE`) を使っており、
  namespace ・code 名のいずれも新規則と異なる。両エラーの併存可否 (SCOPE-AMBIGUOUS を廃止して
  REGISTRY-DUP に一本化するか、別概念として残すか) は実装時に確定が必要。

### PB23 候補集合は registry live 行 ∪ validated scope_path の canonical path (表現差で判定を変えない) [P0]
- 正本: 08 §3.1 手順 1b L156-158『候補集合は registry の live 行に加えて validated scope_path の
  canonical path を含めて数える — registry 未登録の clone を scope_path で指した場合も、既知 live 行と
  合わせて 2 以上なら同じ error (URI 化で optional path が落ちた場合と結果を変えない)』
- 前提: registry に scope_id の live 行が 1 つ存在し、かつ `scope_path` ヒントとして registry
  未登録の別 live clone の path を明示指定する (JSON 形式の pointer で `scope_path` を含む場合と、
  URI 形式で `scope_path` が失われる場合の両方を用意する)。
- 操作: 両表現で同一 scope_id を resolve する。
- 期待: JSON 表現 (scope_path あり) と URI 表現 (scope_path 無し) の両方で、候補が
  「registry live 行 + validated scope_path」の重複除去後 2 以上になるため同じ `KIO-E-REGISTRY-DUP-001`
  候補一覧 error になる — 表現形式の違いで一方が alive、他方が error になってはならない。

### PB24 書き込み系コマンドと online task 起動 (相 1) も live 重複中は fail-closed [P0]
- 正本: 10 §3 L296-299『**live 重複が解消するまでは、当該 scope_id での書き込み系コマンドと online
  タスク起動 (相 1) も `KIO-E-REGISTRY-DUP-001` で fail-closed とする** — device-global
  `batch_requests` の行 (PK に scope_id) を複数 clone が共有し、回復・終端・課金の帰属が混線するため』
- 前提: PB21 の live 重複状態にある scope で、(a) `kio index` (書き込み系)、(b) online Batch 投入の
  相 1 (`04-pipeline.md §5.8`) を試みる。
- 操作: (a)(b) をそれぞれ実行する。
- 期待: (a)(b) いずれも `KIO-E-REGISTRY-DUP-001` で拒否される (読み取り専用の `kio status` のみ拒否
  対象外 — 10 §3 L307)。dedupe (どちらか一方の `.kio` の登録解除・削除) 後に再開できることを案内する。

### PB25 `kio repair --registry-prune` の新設と live clone 検査の除外 [P0]
- 正本: 10 §3 L291-293『**再 init・再発見のどちらも起こらない恒久消滅**... の stale 行は、
  `kio repair --registry-prune` (確認プロンプト付き — 到達不能行を列挙し、live clone 検査 (上記) に
  該当しない行のみ削除)』
- 前提: registry に (a) 到達不能 (`.kio` が実在しない) な stale 行、(b) 到達可能だが scope_id が
  live 重複している行 (PB21 のケース) が混在する。
- 操作: `kio repair --registry-prune` を実行する (確認プロンプトを承認)。
- 期待: (a) のみ削除対象となる。(b) は live clone (単に複数存在するだけで到達可能) であり
  `--registry-prune` の削除対象にはならない (dedupe はユーザーの判断に委ねる — 10 §3 L287
  「どちらを残すかはユーザーの dedupe に委ねる」)。確認プロンプト無しでの実行は拒否される。
  **現行実装との既知の不整合**: `--registry-prune` は main.rs に grep 0 件。

### PB26 [regression-lock] 再 init による stale 退役は既に正しい [P1]
- 正本: 10 §3 L279-283『**stale 登録の退役**: `.kio` を削除して同じ path で `init` し直すと新しい
  `scope_id` が採番される ... upsert の直前に、同一 `kio_path` で `scope_id` が異なる行を削除する』
- 前提: 現行実装 `retire_stale_kio_path` (`kio-index/src/registry.rs` L94-109、テスト L274-306) は
  既にこの規則を満たす。
- 操作: 既存の `.kio` を削除して同一 path で再 init し `kio index` を実行する。
- 期待: 旧 scope_id の登録行が削除され、新 scope_id の行のみが残る — regression-lock (再定義不要)。
  この regression-lock は PB21-25 の fail-closed 化が「同一 path での正当な再 init」まで誤って live
  重複として扱わないことの反証にもなる (再 init 後は旧行が retire 済みで live 重複が発生しないため)。

---

## I. バックアップ最低保全集合の拡大 (U47)

### PB27 truth 区分全行が最低保全集合 (objects/+refs/ のみでは不十分) [P2]
- 正本: 10 §7.5.2 L641-644『**最低保全集合は objects/ と refs/ ではなく、03-data-model.md §4.1 の
  truth 区分の全行** (scope.json / config / tool-lock / tombstones + erase receipts / chunks.jsonl /
  access.jsonl を含む) — これらはいずれも喪失時復旧不能である』
- 前提: `.kio` ディレクトリごとのコピー (MVP 推奨バックアップ手段、専用コマンド無し) を人間が
  実行する運用手順。
- 操作: ドキュメント上のバックアップ手順記述を検査する。
- 期待: 手順が「`objects/` と `refs/` のみで十分」という誤った案内を含まないこと。**この契約は
  P2 (文書検証)**: `kio backup` 相当の専用コマンドが MVP に存在せず (grep 0 件)、`.kio` ディレクトリ
  ごとの単純コピーが唯一の手段であるため、機械検証可能な形での契約は「コピー手段が truth 区分の
  いずれかのファイルを明示的に除外するコード上の frケー致 (allowlist によるファイル選別など) を
  持たないこと」の regression-lock に限られる。**[解釈割れ]**: 専用バックアップコマンドが存在しない
  以上、本項目の実質的な検証手段は運用文書のレビューにとどまり自動テスト化が困難 (§Y-3 参照)。

---

## J. rebuild-db の index_metadata 初期化 + introduction 再導出 (U144)

> 正本: 04 §5.7 L913『再構築完了時は index_metadata へ新 index_generation ULID を採番し、**同じ完了
> Tx で `last_lifecycle_epoch` を現在の lifecycle-epoch counter 値に初期化する**』/ 同 L913『
> publication / association introduction の再導出は chunks.jsonl を正本とする』

### PB28 [regression-lock 寄り] rebuild-db 後の last_lifecycle_epoch は現在値に初期化される (DEFAULT 0 ではない) [P1]
- 正本: 04 §5.7 L913 (上記引用)、05 §3.5 L760-761『`kio repair --rebuild-db` は完了 Tx で現 counter
  値に初期化する — DEFAULT 0 のままの全件誤検出を防ぐ』
- 前提: `.kio/tombstones/lifecycle-epoch` の現在値が `9` の状態で `kio repair --rebuild-db` を実行する。
- 操作: rebuild 完了後の `index_metadata.last_lifecycle_epoch` を検査する。
- 期待: `9` (DEFAULT の `0` ではない) に初期化される。**現状**: `run_repair` の RebuildDb 分岐
  (`main.rs` L970-972) は `rebuild_step3_index` の直後に `recover_index_generation(repo.kio_dir())`
  を呼び、これが `index_metadata` 未初期化なら `purge.read_lifecycle_epoch()` の現在値で
  `ensure_index_metadata` する (L4455-4460) ため、本契約は**実質的に既に満たされている**可能性が高い
  ([適合済みの可能性] — U144 の統合要約は Step 4b Phase 1 適用前の historical inventory 記述であり、Phase 1c
  で `recover_index_generation` が追加された結果、本契約は regression-lock として位置づけを見直す)。
  **[解釈割れ]**: `rebuild_sqlite_index` 自身の DB スワップ Tx とは別の後続呼び出しであり、spec 文言
  「同じ完了 Tx」を文字通り同一 SQL トランザクションと読むなら未達、`run_repair` 全体を包む
  `.kio/.lock` 保持区間 (`main.rs` L932 `_lock = repo.lock_store()`) を「完了」の単位と読むなら
  充足と評価が分かれる (§Y-4 参照)。いずれの読みでも、rebuild と `recover_index_generation` の間で
  crash した場合に次の書込コマンドが自己修復すること (§Y-4 で言及) を別途確認する契約として本項を
  維持する。

### PB29 chunk_publications の event-only 再導出 [P0]
- 正本: 04 §5.7『**publication / association introduction の再導出は chunks.jsonl の tagged
  publication event を正本とする**』
- 前提: `chunks.jsonl` の各 durable creation association には対応する publication event が揃う。
- 操作: `kio repair --rebuild-db` を実行する。
- 期待: `chunk_publications` が chunks.jsonl の publication event 行から再構築され、各
  chunk の introduction commit が event から正しく復元される。各 event は introduction commit の
  tree config と exact normalized unit の認証に通るものだけを採用する。

### PB30 publication event 欠落は fail-closed [P0]
- 前提: durable creation association はあるが、対応する tagged publication event が無い。
- 操作: `kio repair --rebuild-db` を実行する。
- 期待: commit walk や scalar metadata から introduction を推定せず、その association は publication
  relation に現れない。reader は current format violation として fail-closed する。

### PB31 dangling event 行の無視条件 (creation 行/chunk object 欠如 vs introduction commit object 欠如、ref 到達不能 commit は無視しない) [P0]
- 正本: 04 §5.7 L913『生存する creation 行 / chunk object を持たない、**または introduction commit の
  object が store に存在しない** publication event 行は無視する (dangling ... 次回 finalize が冪等に
  再 append する)。**commit object が存在するが ref から到達不能な行 (tag 削除後の orphan /
  disconnected commit — `--at` の正当な明示対象) は無視しない** — commit object は削除されない
  ため、この publication 行は恒久に保持される』
- 前提: publication event 行を (a) 対応する creation 行/chunk object が存在しない、(b) introduction
  commit object が store に存在しない、(c) introduction commit object は存在するが ref から到達不能
  (tag 削除後の disconnected commit)、の 3 パターンで用意する。
- 操作: `kio repair --rebuild-db` を実行する。
- 期待: (a)(b) は dangling として無視される (再構築結果に含まれない)。(c) は無視**されず**保持される
  — `--at`/`--all-history` の解決対象であり続ける。(a)/(b) と (c) を同一に「ref 到達不能なら無視」と
  扱う実装は (c) について契約違反。

---

## K. schema_version の wire 表現統一 (U48)

### PB32 [regression-lock] schema_version は既に MAJOR-only の整数として扱われている [P1]
- 正本: 08 §2.1 L56『`schema_version` | Evidence Pointer schema の version — **wire 上は URI の `sv`
  (§2.3) と同じく MAJOR のみの整数** (semver の MINOR/PATCH は載せない)』
- 前提: 現行実装 `EvidencePointer.schema_version: u64` (`kio-search/src/evidence.rs` L104)、
  `EVIDENCE_POINTER_SCHEMA_VERSION: u64 = 1` (L7)。
- 操作: pointer の inline JSON と URI (`?sv=`) の両方で `schema_version`/`sv` を検証する。
- 期待: 両方とも単純な整数 (MINOR/PATCH の小数点付き表現や文字列は受理されない — `u64` 型と
  `value.parse::<u64>()` (`evidence.rs` L250) がこれを型レベルで保証する) — regression-lock。

### PB33 未知 MAJOR は表現形式に依らず KIO-E-CONFIG-SCHEMA 系 exit 2 で統一拒否 [P0]
- 正本: 08 §8 L503『**未知 MAJOR の拒否は表現形式に依らない**: reader は自己の対応 MAJOR より新しい
  `schema_version` を、URI の `sv` (§2.3) と inline / batch JSON の `schema_version` field のどちらで
  受けても KIO-E-CONFIG-SCHEMA 系 error (exit 2) で拒否する』
- 前提: 未対応の `schema_version`/`sv` (現行対応 MAJOR = 1 に対し `2` 等) を (a) URI の `?sv=2`、
  (b) inline JSON の `"schema_version": 2` で与える。
- 操作: `kio evidence verify`/`kio open`/`kio view` などで pointer を解析する。
- 期待: (a)(b) いずれも同一の `KIO-E-CONFIG-SCHEMA` 系 error・exit 2 で拒否される。**現状**:
  `parse_evidence_pointer_uri` (`evidence.rs` L257-261) は `"KIO-E-CONFIG-SCHEMA-001"` を含む
  `SearchError::Evidence` を返し、`EvidencePointer::validate` (L169-174) は inline JSON 経路で
  `"unsupported evidence schema version"` を返す — 呼び出し元 (`main.rs`) でこれらがいずれも
  `KioError::schema()`/exit 2 に正規化されることを end-to-end で確認する (両経路のエラー文字列自体は
  異なるが、最終的な `error_code` namespace と exit code が一致することが契約の対象)。

---

## L. 表示フィールド canonical 優先・URI opaque (U49)

### PB34 解決成功時は path_at_commit/heading_path 等の表示フィールドが canonical (tree/chunk object 由来) 値を優先する [P0]
- 正本: 08 §2.2 L73-76『表示用 field は、解決が成功した場合は**解決結果の canonical 値 (tree /
  chunk object 由来) を優先して表示し、pointer 入力値と相違するときは入力値を無視する** — 正しい
  必須 tuple に偽の表示 metadata (path / heading / span) を付けた pointer が、alive 判定のままそのまま
  人間向け引用に使われることを防ぐ』
- 前提: pointer の `path_at_commit` に実際の tree entry の path と異なる偽値を仕込む (`heading_path`
  も同様に偽値を仕込む)。raw_hash/tool_profile_hash/chunk_hash 等の必須 tuple は正当。
- 操作: `kio open <pointer>`/`kio view <pointer>` を実行する。
- 期待: 出力される `path_at_commit`/`heading_path` は tree entry・chunk object から得られる canonical
  値であり、pointer 入力の偽値は無視される。**現行実装との既知の不整合**: `PointerResolution`
  (`main.rs` L5928-5933) は `path`/`text`/`temporary`/`commit_shallow` のみを持ち、
  `path_at_commit`/`heading_path` を解決結果として一切出力しない — canonical 優先を適用する対象
  フィールド自体が resolve_pointer_for_cli の出力に存在しない (新規追加が前提の契約)。検索結果構築
  時 (`main.rs` L2054-2055 等) には同名フィールドが存在するが、これは新規発行時の canonical 値であり
  「解決時に pointer 入力値を上書きする」という本契約の対象ではない。

### PB35 receipt-backed shallow pointer は表示 metadata を推測しない [P0]
- 正本: 08 §2.2 / §3.1 手順 2a。tree が欠落した pointer の本文解決では optional
  `path_at_commit` を表示 authority にせず、raw / chunk / HEAD も tree membership の代替にしない。
- 前提: 有効 shallow receipt により tree object が欠落する commit を指す pointer を用意し、
  `path_at_commit` にもっともらしい値を仕込む。
- 操作: `kio open` / `kio view` を実行する。
- 期待: 本文・表示 metadata は返さず `KIO-E-COMMIT-SHALLOW-001` で fail-closed する。tree を
  backup から復元してから再実行するまで、pointer 入力値を canonical path として表示しない。

### PB36 [regression-lock] Evidence Pointer URI は opaque で authority の大文字小文字を保存する [P1]
- 正本: 08 §2.3 L102-104『**URI は opaque として扱い、authority 位置 (scope_id) の大文字小文字を
  保存する** — 一般 URI 正規化 (authority の小文字化) を適用してはならない。lookup は case-sensitive
  (registry の TEXT キーと一致 — ULID は大文字表記が正)』
- 前提: 大文字を含む scope_id (ULID、例 `scope_01J8ZQABCDEFGHJKMNPQRS`) を持つ pointer を URI 化し、
  再度パースする。
- 操作: `evidence_pointer_to_uri`/`parse_evidence_pointer_uri` (`evidence.rs` L227-290) の往復を行う。
- 期待: scope_id の大文字小文字が保持される — 現行実装は `.to_lowercase()` 等の正規化を一切行わず
  `parts[0].to_owned()` (L275) でそのまま格納するため、regression-lock として固定する。registry
  lookup (`RegistryDb::lookup_scope_id`) の SQL も `TEXT` 型の完全一致 (`WHERE scope_id = ?1`) であり
  case-insensitive 照合を行わない。

---

## M. object URI type=image 限定 (U50)

### PB37 object URI の受理 type を type=image のみに限定する (5 型受理からの縮小) [P0]
- 正本: 08 §2.3 L110-113『**MVP で発行・受理される object URI は type=image のみ** — 発行面は
  07-adapter-spec.md §5.2 の画像参照置換だけで、他 type の URI は発行されない。受理側も image 以外は
  拒否 — 06-cli-spec.md §1.1 手順 1a。type を追加する場合は 06 §1.1 に open semantics を定義してから』
- 前提: `kio://<scope_id>/object/<type>/<hash>` 形式の object URI を type = (a) `image`、(b) `raw`、
  (c) `chunk`、(d) `normalized`、(e) `prepared` でそれぞれ用意する。
- 操作: `kio open`/`kio view` に各 URI を渡す。
- 期待: (a) のみ受理される。(b)(c)(d)(e) はいずれも拒否される (`KIO-E-CONFIG-...`/invalid usage 系、
  exit 2)。**現行実装との既知の不整合**: `parse_object_uri` (`main.rs` L7299` VALID_TYPES`) は
  `["raw", "image", "chunk", "normalized", "prepared"]` の 5 型を parse 時点で受理しており、
  `resolve_object_uri` (L7326-7358) も raw/image/chunk/prepared の 4 型を実際に解決可能にしている
  (`normalized` のみ「単一 hash では解決不能」として別エラーメッセージで拒否される) — image 以外を
  parse 段階で拒否する新規則と現状は大きく乖離する。

### PB38 [解釈割れ・P2] fork 複製 scope 内の旧 scope_id object URI は自 store の同一 hash で解決する [P2]
- 正本: 08 §2.3 L114-117『fork 複製 (`kio import --as-new-scope`) 内の旧 scope_id を含む object URI
  は、文脈 store に該当 hash の object があれば自 store で解決する (06-cli-spec.md §1.1 手順 1a —
  hash が identity)』
- 前提: `kio import --as-new-scope` 自体が現状未実装 (grep 0 件、Phase 4+ 相当の周辺機能)。
- 操作: (実装前提の記述のみ) — 自 store に同一 image_hash を持つ複製 scope で、複製元の (異なる)
  scope_id を含む object URI を解決しようとする。
- 期待: **[解釈割れ]** `kio import --as-new-scope` が存在しない現状、この規則を実機で検証する経路が
  無い。本契約は将来 `kio import --as-new-scope` が実装された際に満たすべき期待値を記録するのみで
  あり、P2 (Phase 4+ 依存、現時点では検証不能) として扱う。

---

## N. receipt-backed shallow tree の fail-closed 境界 (U51)

### PB39 [regression-lock] open/view は shallow tree を raw/chunk で代替解決しない [P1]
- 正本: 08 §3.1 手順 2a。tree が欠落し valid shallow receipt がそれを正当化するとき、本文を返す
  resolver は membership / profile / gen / 時点帰属を検証できないため成功へ降格しない。
- 前提: valid shallow receipt に対応する tree object を欠落させた commit を指す pointer を用意する。
- 操作: shallow commit を指す pointer を `kio open` / `kio view` で解決する。
- 期待: 両方が `KIO-E-COMMIT-SHALLOW-001` (exit 1) で失敗する。chunk_hash からの直接本文解決、
  current HEAD / SQLite cache への fallback、tree entry 検証の省略はいずれも発生しない。tree を backup
  から復元した後だけ full-tree resolver が通常の検証を再開する。

### PB40 `kio evidence verify --strict` は shallow 解決を alive でなく unverifiable(exit 3) に降格する [P0]
- 正本: 08 §4.3。本文を返さない verify は shallow を verify-only の `commit_shallow` として観測し、
  `--strict` では **unverifiable (exit 3)** として返す。
- 前提: shallow commit を指す pointer で `kio evidence verify <pointer> --strict` を実行する。
- 操作: 上記コマンドを実行し `status` と exit code を検査する。
- 期待: `status: "unverifiable"`, `details.reason: "commit_shallow"`, exit code 3。**現行実装との
  既知の不整合**: `verify_pointer_for_cli` (`verify_objects.rs` L184-195) は shallow でも
  `status: "alive"` (`commit_shallow: true` を details に含むのみ) を返し、`run_evidence` の strict
  判定 (L41-45) は `status.and_then(as_str) != Some("alive")` という**単純な alive/非 alive 二値判定**
  であるため、shallow は `status=="alive"` の条件に該当し **exit override が一切適用されず exit 0
  のまま**返る — spec が要求する exit 3 への降格が完全に欠落している (単なる「reason 別 exit の未実装」
  ではなく、shallow を strict 判定から見逃す実質的なバグ)。

### PB41 --strict 判定はステータス種別だけでなく reason 別 exit テーブルに従う [P0]
- 正本: 08 §4.3 L365『exit は reason の再試行可能性に従い分岐する — commit_shallow のみなら 3
  (unshallow で解消し得る)、tree_v1 / manifest_missing を 1 件でも含めば **4** (恒久 — 再試行で進展
  しない)』
- 前提: `--batch` 以外の単発 `--strict` verify で、`status="unverifiable"` かつ `details.reason` が
  (a) `commit_shallow` のみ、(b) `tree_v1`、(c) `manifest_missing` の 3 パターン (U54/U55 実装後を
  前提とする)。
- 操作: 各パターンで `kio evidence verify <pointer> --strict` を実行する。
- 期待: (a) は exit 3。(b)(c) は exit 4。`run_evidence` の現行ロジック (`__exit_code: 4` を
  無条件付与、L41-45) は reason 別分岐を一切持たないため、PB40 と合わせて `--strict` の exit 判定
  ロジック全体を reason テーブル駆動へ書き換える必要がある。

---

## O. 手順 4: 同一 raw_hash 複数 entry の決定的選択 (U52)

### PB42 tool_profile_hash binding によるentry 選択 (現状の「最初の raw_hash 一致」からの置換、両呼び出し元パラメタ化) [P0]
- 正本: 08 §3.1 手順 4 L172-176『同一 commit 内に同一 raw_hash が複数 path へ配置されている場合...
  **pointer の tool_profile_hash と一致する binding の entry を選ぶ**』
- 前提: 同一 commit の tree に同一 raw_hash を持つ entry が 2 つ存在し、異なる `normalize.
  tool_profile_hash` を持つ (`path=a.md` は `tool_profile_hash=T1`、`path=b.md` は `T2`)。pointer の
  `tool_profile_hash=T2`。
- 操作: (a) `kio open`/`kio view`/`kio restore` (`resolve_pointer_for_cli`, `main.rs` L6216-6257)、
  (b) `kio evidence verify` (`verify_pointer_for_cli`, `verify_objects.rs` L113-138) の両方で解決する。
- 期待: (a)(b) いずれも `T2` (pointer の tool_profile_hash) に binding する entry (`b.md`) が選ばれる。
  **現行実装との既知の不整合**: 両関数とも `tree.entries.iter().find(|entry| entry.raw_hash ==
  pointer.raw_hash)` で raw_hash 一致の**最初の 1 件**を機械的に採用しており (main.rs L6218-6221,
  verify_objects.rs L114-118)、binding 選択ロジックが無い — 偶然 `a.md` が先に列挙された場合、
  `tool_profile_hash` 不一致として `invalid_pointer_identity_error` に落ちる (誤って corruption
  扱いになる)。

### PB43 同一 binding の複数 entry は path の UTF-8 byte 順最小を決定的に選ぶ [P0]
- 正本: 08 §3.1 手順 4 L176『同一 binding の entry が複数残る場合は **path の UTF-8 byte 順最小の
  entry を決定的に選ぶ** (05-runtime.md §1.7 の `path_at_commit` と同じ規則 — 表示もこの canonical
  path を使い、pointer 入力の optional path は使わない)』
- 前提: 同一 raw_hash・同一 tool_profile_hash (binding も同一) の entry が `path="z.md"` と
  `path="a.md"` の 2 つ存在する (同一 commit 内の複数 path 配置)。
- 操作: `kio open`/`kio view` で解決する。
- 期待: `path="a.md"` (UTF-8 byte 順最小) が決定的に選ばれる。実行順序 (tree.entries の格納順) に
  依存する非決定的な選択は契約違反。

### PB44 一致 entry ゼロ件は手順 5-7 短絡・KIO-E-STORE-CORRUPT-001 (現状の PURGE-NOT-FOUND-001 誤りをパラメタ化で是正) [P0]
- 正本: 08 §3.1 手順 4 L176-178『一致 entry が無ければ手順 5〜7 を実行せず `KIO-E-STORE-CORRUPT-001`
  (not_found 扱い — 手順 8 の不一致処理と同じ終端) へ短絡する』
- 前提: pointer の raw_hash に一致する tree entry が commit の tree に**1 件も存在しない** (DAG は
  purge で書き換えられないため — 02-philosophy §2.4/10 §7 U29 — これは genuine corruption であり
  purge の正常な帰結ではない)。
- 操作: (a) `resolve_pointer_for_cli` (open/view/restore)、(b) `verify_pointer_for_cli` (evidence
  verify) の両方で解決する。
- 期待: (a)(b) いずれも `KIO-E-STORE-CORRUPT-001` で終端し、tombstone/marker の確認 (手順 5) を
  一切行わない。**現行実装との既知の不整合**: (a) は entry 不在時に `enforce_canonical_tombstone_only`
  を呼んでから `purge_not_found_error` (`KIO-E-PURGE-NOT-FOUND-001`) を返し (main.rs L6222-6231)、
  (b) も同様に `read_tombstone` を経て `not_found_verify_output` (`KIO-E-PURGE-NOT-FOUND-001`) を
  返す (verify_objects.rs L119-124) — 両方とも marker 確認を経由してから誤ったエラーコード
  (PURGE-NOT-FOUND ではなく STORE-CORRUPT が正しい) で終端しており、本契約が要求する「手順 5 を
  実行せず短絡する」という順序そのものにも反する (tombstone なら tombstone を返す、という現状の
  救済的挙動は spec の「一致 entry が無ければ手順 5〜7 を実行せず」という明確な短絡規則と衝突する)。

---

## P. 手順 6a: v2/v3 tree の時点帰属検証 (U54)

> 正本: 08 §3.1 手順 6a L207-218。前提として PB04 の `normalize.manifest_hash` field 追加が必要。

### PB45 chunk の unit_key が対象 manifest で status=done であることの検証 [P0]
- 正本: 08 §3.1 手順 6a L207-210『entry の normalize.manifest_hash が指す manifest object を読み、
  chunk の unit_key が当該 manifest で status=done であることを検証する ... done でない unit の chunk
  は当該 commit 時点に存在しない (same-gen retry の後着 chunk を過去 commit の証拠として返さない →
  not_found)』
- 前提: tree entry の manifest 内で、pointer の chunk が指す unit_key が (a) `status=done`、(b)
  `status` が done 以外 (in_flight 等、same-gen retry の後着を模したもの)。
- 操作: `kio open`/`kio view` で解決する。
- 期待: (a) は手順 7 (本文取り出し) へ進む。(b) は not_found として終端する (chunk object 自体は
  存在しても、当該 commit 時点の証拠としては採用しない)。

### PB46 chunk publication introduction の ancestor-or-equal 検証 [P0]
- 正本: 08 §3.1 手順 6a L210-215『v2/v3 tree ではさらに、chunk の publication と config association
  の introduction (04-pipeline.md §4.1) が pointer の commit の ancestor-or-equal であることも
  検証する ... manifest で done でも当該 commit 時点で未公開の chunk を証拠にしない (cache 参照の
  ため、association の**不在**による失敗は corruption ではなく not_found — rebuild 後に再評価
  できる)』
- 前提: chunk の publication introduction commit が pointer の commit の
  (a) ancestor-or-equal、(b) descendant (= pointer の commit より後に公開された = 未来の chunk)。
- 操作: `kio open`/`kio view` で解決する。
- 期待: (a) は証拠として採用 (手順 7 へ進む)。(b) は not_found (corruption ではない — introduction
  レコードの不在/未来公開は「まだ確立していない association」であり fsck の corruption 対象にしない)。

### PB47 config association の introduction 検証範囲限定 (対象 tree の chunking_config_hash のみ) + sqlite.db 利用不能時の分離 [P0]
- 正本: 08 §3.1 手順 6a L211-213『config association は**対象 tree の `chunking_config_hash` のもの**
  — 05-runtime.md §1.6 の検索側と同一の絞り込み。別 config の association は当該 commit への帰属を
  証明しない』/ L216-218『**sqlite.db 自体の不在・再構築中はこの検証を実行できない — not_found
  ではなく `KIO-E-INDEX-REBUILDING-001` の再構築要求を返し**、検証不能を「不在の確定」と混同しない』
- 前提: 対象 chunk が chunking_config_hash=`Ca` の下では introduction 済みだが `Cb` (別 config) の
  下でのみ introduction されている状態。別途、sqlite.db が存在しない (未初期化) 状態。
- 操作: (a) tree の chunking_config_hash が `Ca` の commit を pointer で解決する。(b) sqlite.db が
  存在しない状態で同じ pointer を解決する。
- 期待: (a) は `Cb` の association では帰属を証明できないため not_found (`Ca` 側の association が
  無ければ)。(b) は not_found ではなく `KIO-E-INDEX-REBUILDING-001` (exit 3、再構築要求) を返す —
  検証不能と不在確定を混同しない。

---

## Q. 手順 6b: manifest 欠落の説明範囲限定・降格・resurrection link 解決 (U55)

### PB48 manifest 欠落の説明範囲限定 (in_commit 以前限定) と manifest_missing 降格 [P0]
- 正本: 08 §3.1 手順 6b L222-228『entry の manifest object が purge により欠落している場合 (... 説明
  範囲は fsck と同一 ...): 当該 purged / erased event の `in_commit` **以前**の commit が参照する
  closure に限る — pointer の commit がこの範囲外なら 6b を適用せず KIO-E-STORE-CORRUPT-001
  (not_found 扱い) とする ... 手順 2a と同じ直接解決へ降格し、レスポンスに `manifest_missing: true`
  を付す』
- 前提: raw_hash `X` の canonical final event = `purged` (in_commit=`Cp`)。manifest が欠落した pointer
  の commit が (a) `Cp` 以前、(b) `Cp` より後 (再作成・再公開分)。
- 操作: `kio open`/`kio view` で解決する (PB04/PB45 実装後を前提)。
- 期待: (a) は直接解決へ降格し `manifest_missing: true` を伴って alive 相当を返す (手順 4 の tree
  entry 照合は実施 — 手順 8 の entry 系照合も維持)。(b) は `KIO-E-STORE-CORRUPT-001` (not_found 扱い)。

### PB49 retired event の resurrection_commit リンク経由の代替解決 [P0]
- 正本: 08 §3.1 手順 6b L230-250『retired event に `resurrection_commit` があれば、そのリンク先
  commit の publication を参照して本文を解決し alive を返してよい ... **resurrection link 経由の
  解決は、当該 retired event の resurrection_commit を基準に検証する** ... リンクとして有効なのは
  **canonical final event (手順 5) が retired の場合の当該 event のみ**』
- 前提: pointer が purge **前**の古い commit `C1` を指す。tombstone の canonical final event が
  `retired` (resurrection_commit=`C2`)。`C1` 時点の tree entry の manifest は purge により欠落済み。
- 操作: `kio open`/`kio view` で `C1` を指す旧 pointer を解決する。
- 期待: 手順 6b の直接解決 (PB48) だけでは `C1` 時点の manifest 欠落が説明されない場合でも、
  `resurrection_commit=C2` のリンクを辿り、`C2` 側の publication (chunk publication / config
  association introduction が `C2` を基準に ancestor-or-equal であること) を検証して alive を返す。
  **現行実装との既知の不整合**: `resolve_pointer_for_cli` (`main.rs` L6170-6356) は
  `canonical.event.resurrection_commit` を一切参照せず、常に pointer 自身の `commit` (=`C1`) のみを
  基準に解決する — resurrection link 経由の代替解決パスが存在しない。これは単に「manifest_hash が
  無いから」では説明できない独立した機能欠落であり (U55 のうち manifest_hash 前提を要しない部分)、
  PB48 とは別に固定する必要がある。

### PB50 commit_shallow と manifest_missing の相互排他性 [P1]
- 正本: 08 §3.1 手順 6b L251-253『unverifiable になるのは manifest done 検査のみ。`manifest_missing`
  は 6b を実行できる non-shallow 解決でのみ設定される — shallow (2a) は 6b を適用しないため
  `commit_shallow` とは**相互排他**(schema 上は独立 field だが同時に true にならない)』
- 前提: PB48/PB49 実装後、shallow commit かつ manifest 欠落という状態を人為的に構築しようとする
  (shallow は tree 自体が無いため tree entry 参照を要する 6b に本来到達しない)。
- 操作: 各種 pointer 解決結果の `commit_shallow`/`manifest_missing` フィールドを検査する。
- 期待: `commit_shallow: true` の応答は `manifest_missing` を含まない (または `false`)。逆も同様。
  両方 true になる応答が生成された場合、それ自体が実装のバグとして扱われる (schema 上両立可能に
  見えても、意味論上は排他)。

---

## R. 手順 8: defense-in-depth の整合再検証 (U56)

### PB51 手順 4 の選択とは独立した終端再検証 (entry.normalize.tool_profile_hash 再一致・gen 一致) [P0]
- 正本: 08 §3.1 手順 8 L255-261『手順 4-6 を経た場合はさらに **tree entry の
  normalize.tool_profile_hash が pointer の tool_profile_hash と一致し**、chunk object の gen が
  tree entry の gen と一致することを検証する (手順 4 の tool 一致選択とは**独立に**、終端で entry 側
  の tool 一致を再検証する defense-in-depth — この postcondition を欠くと、手順 4 の選択が破損・
  改変した store 上で迂回された場合に... 別 tool の chunk が当該 commit の証拠として通ってしまう)』
- 前提: PB42 (手順 4 の binding 選択) が実装された状態で、選択ロジックが (仮に) バグにより
  誤った entry を選んでしまうケースをフォルトインジェクションで模擬する (テスト用に手順 4 の
  選択結果を差し替え、`tool_profile_hash` が pointer と一致しない entry を意図的に手順 6 以降へ
  渡す)。
- 操作: この模擬状態で手順 7-8 相当のコードパスを実行する。
- 期待: 手順 8 の再検証が独立して `tool_profile_hash` 不一致を検出し `KIO-E-STORE-CORRUPT-001` で
  終端する — 手順 4 の選択ロジックの正しさに依存せず、終端で必ず再検証が働くこと。**現状**: 現行
  実装 (`main.rs` L6242-6250 の選択時チェックと L6304-6317 の終端チェック) は同じ `entry_gen`/
  `tool_profile_hash` 変数を使い回しており、選択時チェック (手順 4 相当) と終端チェック (手順 8
  相当) が構造的に同一ロジックの再利用であって、真に独立した 2 段検証にはなっていない — 本契約は
  この構造上の疑義を明示するためのフォルトインジェクション契約である。

### PB52 shallow 経路は tree membership を検証できないことの明示 (commit_shallow フラグが担う) [P1]
- 正本: 08 §3.1 手順 8 L264-266『**shallow 経路 (2a) は tree membership を検証できない** — この
  限界は `commit_shallow: true` が表明し、`--strict` verify は shallow 経路の解決を alive でなく
  unverifiable (exit 3) として返す』
- 前提: shallow commit を指す pointer。
- 操作: `kio open`/`kio view`/`kio evidence verify` で解決する。
- 期待: 応答に `commit_shallow: true` が含まれ (既存、PB39 で regression-lock 済み)、`--strict`
  evidence verify は PB40 の unverifiable 降格を適用する — 本契約は PB39/PB40 の「手順 8 の観点からの
  相互参照」であり、新規検証は課さない (§K 表の参照専用に近い軽量確認)。

---

## S. evidence verify status の 6 値 union 化 (U57, U53 の verify 部分)

> 正本: 08 §4.3 L354-359『`{"status": "alive" | "tombstoned" | "not_found" | "scope_unreachable" |
> "unverifiable" | "registry_duplicate", "details": {...}}`』。**現状**: `verify_pointer_for_cli`
> (`verify_objects.rs`) は `alive`/`tombstoned`/`not_found` の 3 値のみ返す (L184, L198-207,
> L209-218)。`"scope_unreachable"`/`"unverifiable"`/`"registry_duplicate"` は crates/ 全体で文字列
> リテラルとして grep 0 件 (本コマンドで確認済み)。

### PB53 scope_unreachable は構造化 status であり raw error ではない [P0]
- 正本: 08 §4.3 L356 (union 定義) / §3.2 L288-291『scope の .kio に到達できない: scope_unreachable
  — scope_path 不達かつ scope_registry に scope_id 未登録 → KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001』
- 前提: 存在しない scope_id・到達不能な scope_path を持つ pointer。
- 操作: `kio evidence verify <pointer>` (`--strict` 無し) を実行する。
- 期待: コマンドは exit 0 で完了し、JSON body の `status` が `"scope_unreachable"` である
  (`details` に `error_code: KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001` 相当を含めてよい)。**現行実装
  との既知の不整合**: `verify_pointer_for_cli` の冒頭 `resolve_scope_target` (L97) が失敗すると
  `?` 演算子でそのまま `Err` が伝播し、`run_evidence` (L31-47) はこれを **CLI トップレベルのエラー**
  として exit 非 0 で終了する (構造化 `status` フィールドを持つ成功レスポンスにならない) — verify
  コマンド全体が「status で失敗を表現する」という 08§4.3 の設計と異なり「コマンド自体が失敗する」
  形になっている。

### PB54 registry_duplicate は候補一覧を伴う status [P0]
- 正本: 08 §4.3 L356, L366『live clone 重複は status `registry_duplicate` (候補一覧つき、exit 3 —
  §3.1 手順 1)』
- 前提: §H (PB21) の live 重複状態にある scope_id を持つ pointer。
- 操作: `kio evidence verify <pointer>` (strict 有無それぞれ) を実行する。
- 期待: `status: "registry_duplicate"`、`details` に候補 `.kio` path の一覧を含み、exit code は
  strict の有無に関わらず 3 (retryable — PB56 の exit テーブルの一部)。現状はこの経路が
  `KIO-E-EVIDENCE-SCOPE-AMBIGUOUS-001` の raw error になる (PB53 と同型の不整合 — §H の
  fail-closed 化 (PB21-22) が先行実装されて初めて再現可能になるケースを含む)。

### PB55 unverifiable status と reason union (commit_shallow/tree_v1/manifest_missing) [P0]
- 正本: 08 §4.3 L361-365『`unverifiable` は `--strict` 時の「時点帰属を検証できない解決」であり、
  `details.reason` で区別する: `commit_shallow` / `tree_v1` (手順 6a) / `manifest_missing` (手順 6b)』
- 前提: PB40 (commit_shallow)・§P (U54, tree_v1)・§Q (U55, manifest_missing) がそれぞれ実装された
  3 パターンの pointer で `--strict` verify する。
- 操作: 各パターンで `kio evidence verify <pointer> --strict` を実行する。
- 期待: 3 パターンいずれも `status: "unverifiable"`、`details.reason` がそれぞれ対応する値。3 値が
  閉じた enum であり、他の自由文字列が現れないこと。

### PB56 exit code の reason 別テーブル (3 vs 4) 統合確認 [P0]
- 正本: 08 §4.3 L365『exit は reason の再試行可能性に従い分岐する — `commit_shallow` のみなら 3
  (unshallow で解消し得る)、`tree_v1` / `manifest_missing` を 1 件でも含めば **4** (恒久)』/ L373-376
  『`--strict`: tombstoned / not_found を **error** として扱う ... exit code: ... tombstoned /
  not_found があれば **4** ... **scope_unreachable のみ**の失敗は **3**』
- 前提: PB40/PB53-55 で構築した各 status (`tombstoned`, `not_found`, `scope_unreachable`,
  `unverifiable`×3 reason, `registry_duplicate`) をそれぞれ単独で `--strict` verify する。
- 操作: 各ケースで exit code を集計する。
- 期待: `alive` → 0。`tombstoned`/`not_found`/`unverifiable(tree_v1)`/`unverifiable(manifest_missing)`
  → 4。`scope_unreachable`/`unverifiable(commit_shallow のみ)`/`registry_duplicate` → 3。この 2 群
  (恒久 vs 再試行可能) を単一の `status != "alive" => 4` という現行ロジック (`run_evidence` L41-45)
  で判定することはできない — reason/status ごとの exit テーブルへの全面書き換えが必要。

### PB57 sqlite.db 利用不能時は status ではなく command-level KIO-E-INDEX-REBUILDING-001 [P1]
- 正本: 08 §4.3 L367-370『**sqlite.db が不在・利用不能の場合は status ではなく command-level の
  retryable error `KIO-E-INDEX-REBUILDING-001` (exit 3)** — 検査は完了していないため --strict なしでも
  0 を返さない (再構築中でも旧 sqlite.db が読めるなら通常応答)』
- 前提: `.kio/index/sqlite.db` が存在しない (初回 index 未完了、または rebuild-db 進行中で旧 DB も
  無い) 状態。
- 操作: `kio evidence verify <pointer>` (strict 無し) を実行する。
- 期待: `status` フィールドを持つ成功レスポンスではなく、`KIO-E-INDEX-REBUILDING-001` の command-level
  error (exit 3) を返す。`--strict` の有無に関わらずこの挙動は変わらない (「--strict なしでも 0 を
  返さない」)。**この規則は §P (PB47) の「sqlite.db 不在時は手順 6a を検証不能」規則と対をなすが、
  別の適用場面 (手順 6a は既に commit/tree/manifest 解決後の話、本契約は verify コマンド冒頭の DB
  可用性そのもの) であることに留意する。**

---

## T. purge journal 進行中の evidence verify 拒否 (U58)

### PB58 [regression-lock 寄り] active journal 中の verify は評価を行わず KIO-E-PURGE-JOURNAL-ACTIVE-001 (exit 3) [P1]
- 正本: 08 §4.3 L387-389『active purge journal 中の verify は評価を行わず、KIO-E-PURGE 系 retryable
  (exit 3) を返す (05-runtime.md §3.5 の読取系規約 — marker 耐久化後・削除完了前の窓で「削除対象が
  alive」と誤答しないため)』
- 前提: `.kio/purge/journal` が active な状態 (`PurgeState::read_journal()` が `Some` を返す)。
- 操作: `kio evidence verify <pointer>` を実行する。
- 期待: `status` を含む通常レスポンスではなく `KIO-E-PURGE-JOURNAL-ACTIVE-001` (retryable, exit 3)
  で拒否される。**現状**: `verify_pointer_for_cli` 冒頭の `ReadBarrierCheckpoint::open` (L101、
  `main.rs` L6963-6973) が `purge.read_barrier_active()` を検査し、true なら
  `purge_journal_active_error()` (`KIO-E-PURGE-JOURNAL-ACTIVE-001`, `ExitCode::PartialFailure`=3) を
  返す — これは Step 4b Phase 1c で追加された §I read barrier (LC52-56) の副産物として**既に本契約を
  満たしている可能性が高い**([適合済みの可能性] — historical inventory の元記述はこの Phase 1c 追加前のコード
  読解に基づく)。本契約は regression-lock として維持し、raw_hash 単位ではなく scope 全体の active
  journal 検出であること (対象 raw_hash と無関係な purge でも拒否されること) を追加で確認する。

---

## U. retarget fail-closed 強化 + match_method 互換性分類 (U59, U60 — 実装は Phase 4+)

> `kio evidence retarget` の実装自体は 08§5『retarget の実装は Phase 4+』により本 Phase の対象外。
> 以下は実装着手時に満たすべき契約を先行して固定するものであり、現時点でのコード変更を要求しない。

### PB59 完全一致の複数候補は fail-closed (先勝ち禁止) [P1]
- 正本: 08 §5 L429『**完全一致が複数 chunk に成立する場合も一意に定まらないため
  `KIO-E-EVIDENCE-RETARGET-AMBIG-001` (fail-closed — 先勝ちで選ばない)**』
- 前提: (実装後を想定) 旧 pointer の `heading_path` が新 snapshot の 2 つ以上の chunk と完全一致する。
- 操作: `kio evidence retarget <old_pointer>` を実行する。
- 期待: `status: "ambiguous"`, `error_code: "KIO-E-EVIDENCE-RETARGET-AMBIG-001"`、`candidates` に
  複数候補を含む。実装順序に依存して先頭候補を無条件採用することは契約違反。

### PB60 fuzzy 対応は text alignment 成立領域内限定・retargeted_from はレスポンス直下 [P1]
- 正本: 08 §5 L429『**span 重なり率は、新旧の normalized text 間で text alignment が成立した領域内
  でのみ用いる** — 異なる tool_profile の unit-local byte offset は共通座標を持たないため直接比較
  しない。alignment が成立しない場合は対応なし (ambiguous — fail-closed)』/ 08 §5 L407-412
  『`retargeted_from` が新 pointer オブジェクト内部ではなく**response 直下 (pointer 外) のトップ
  レベルフィールド**』
- 前提: (実装後を想定) 新旧 tool_profile の unit-local byte offset が異なる 2 つの chunk 集合を用意し、
  (a) text alignment が成立する領域、(b) 成立しない領域でそれぞれ fuzzy 対応を試みる。
- 操作: `kio evidence retarget <old_pointer>` を実行する。
- 期待: (a) は `heading_path_fuzzy` で対応付けが成立しうる。(b) は ambiguous (fail-closed)。成功時の
  JSON は `{"status": "retargeted", "new_pointer": {...}, "retargeted_from": "<old_pointer>", ...}`
  の形で `retargeted_from` が `new_pointer` の**外側**のトップレベルに置かれる (内部にネストしない)。

### PB61 [P2・文書契約] match_method の追加は MINOR 相当 (resolver 入力ではない) [P2]
- 正本: 08 §5 L429 末尾『pointer schema 本体への field 追加ではない』(match_method は retarget
  response 限りの field で resolver 入力ではないため、旧実装は §8 の未知フィールドと同様に未知値を
  無視できる)。
- 前提: 将来 `match_method` に `semantic_fingerprint` 等の新値が Phase 4+ で追加される。
- 操作: 旧バージョンの reader が新しい `match_method` 値を含む retarget response を受け取る。
- 期待: 旧 reader は未知の `match_method` 値でエラーにならない (無視できる) — pointer 本体
  (schema_version) の MAJOR bump を要求しない。本契約は match_method 自体が未実装のため純粋に
  文書上の互換性方針の確認にとどまる。

---

## V. `kio evidence verify --batch` (U61)

### PB62 [Phase 4 milestone 6 で置換済み] typed batch verify contract [P1]
- 正本: 08 §4.3 の bounded JSONL / versioned output / all-or-nothing / final barrier 契約。
- 前提: `EvidenceArgs` は typed nested Clap subcommand であり、pointer positional と `--batch` は
  exactly-one / 相互排他である。手書き parser や予約拒否経路は存在しない。
- 操作: single / batch の exactly-one、malformed/blank/unknown/nonobject/invalid UTF-8、全上限、
  全 status、strict priority、command-level error、duplicate/order、multi-scope、unsafe link、
  deterministic output、cache final recheck、single parity を Rust integration/unit test で検査する。
- 期待: `crates/kio-cli/tests/step4b_p2b_contract.rs` の PB62 群と production helper の unit test が
  canonical contract を直接固定する。本節は旧予約拒否の static-source regression lock を置換した
  非実行の受け入れ記録である。

---

## W. path_at_commit の legacy tree 例外 (U62)

### PB63 [解釈割れ・P2] legacy tree 由来 entry の path 区切り例外は識別手段が未確定 [P2]
- 正本: 08 §2 L50『`path_at_commit` は... パス区切り (`/`) を含まない (03 §3。**例外**: 03 §3 の
  forward 規則以前に作られた検証済み legacy tree 由来の entry に限り、区切り等を含む旧 path をそのまま
  保持する — 表示専用であり resolver 入力には使わない）』
- 前提: `TreeEntry::validate()` (`kio-core/src/dag.rs` L44-71) は `is_logical_direct_child` (L92-98、
  `/` 等を含む path を無条件拒否) を経由なく常に適用する — legacy tree 由来かどうかを判定する分岐が
  無い。
- 操作: 区切りを含む legacy path を持つ (実装前提の) tree entry を validate する。
- 期待: **[解釈割れ]** spec は「03§3 の forward 規則制定**以前**に作られた検証済み legacy tree」を
  例外条件とするが、実装が個々の tree entry を検証する時点で「この tree がいつ作られたか (forward
  規則制定前かどうか)」をどう判定するのか、識別に使う具体的な metadata (commit の created_at
  閾値か、専用 schema バージョンフラグか) を spec 本文は明示しない。本項は §Y-5 で規範側への確認を
  要請するにとどめ、実装方式を先取りして決めない。表示面 (`path_at_commit` の出力自体) が現状存在
  しない (PB34 参照) ため、適用対象そのものが未整備でもある。

---

## X. evidence verify の canonical validator 統一 (Phase 1 引き継ぎ — LC21 原則)

> 発注側指示: 「Phase 1 引き継ぎの **evidence verify の同一 validator 化 (LC21 原則 — verify も
> canonical final event 正本化を共有)**」。**LC21** は「検証
> **失敗** marker は入口非依存で corruption (fsck/resolver/re-purge 統一)」を既に契約化済みだが、
> これは malformed marker の corruption 判定に限定した narrow な契約である。本節はそれを踏み台に、
> **検証成功マーカー同士の canonical 集約そのもの** (LC8-10 の 4 分岐アルゴリズム全体) が
> `kio evidence verify` でも共有されることを契約化する — 08§3.1 手順 5 は resolver 系全般
> (`search`/`open`/`view`/`evidence verify`/`restore`/`diff`/`inspect`) に等しく適用される規範で
> あり、`evidence verify` だけ別の (単純化された) 判定ロジックを持ってよいという根拠は spec 上どこにも
> 無い。

**現状の構造 (2026-07-22 時点でのコード読解)**: `main.rs` の `enforce_canonical_marker_barrier`
(L7055-7086、doc comment 内に自己申告あり) は Step 4b の本セッションで `open`/`view`/`restore` にのみ
配線され、`search`/`log`/`diff`/`inspect`/`evidence verify` の 5 コマンドは旧来の
`enforce_purge_read_barrier`(L7137-7145)/`verify_pointer_for_cli` 自身の `read_tombstone(...).is_some()`
(単一 marker の `tail().kind == Purged` 判定、tombstone のみ・erase receipt 不参照) に依存し続けている
— コード自身の doc comment が "Known residual scope gap" として明記する。本節はこの gap のうち
`evidence verify` 分を対象として固定する (`search`/`log`/`diff`/`inspect` は H 領域が別途扱う)。

### PB64 LC10 worked example の evidence verify 経由再現: tombstone purged@10 + receipt retired@11 は alive [P0]
- 正本: 08 §3.1 手順 5 L187-190『(§3.2 の解決成功条件... をここで検査する — (i) が個別 marker の
  末尾で先に短絡しない: 例えば tombstone 末尾 purged@epoch10 + receipt 末尾 retired@epoch11 は
  canonical = retired であり (iii) 側)』(**LC10** と同一
  シナリオ、本契約はそれを `kio evidence verify` 経由で再現する)。
- 前提: raw_hash `X` の tombstone 末尾 event = `{kind:"purged", lifecycle_epoch:10}`、erase receipt
  末尾 event = `{kind:"retired", lifecycle_epoch:11}` (両方とも構造検証通過)。raw object `X` は CAS に
  存在する (retired の前提を満たす)。
- 操作: `kio evidence verify <X を指す pointer>` を実行する。
- 期待: `status: "alive"` を返す (canonical final event = `retired` であり、tombstone 単独の末尾が
  `purged` であることに短絡しない)。**現行実装との既知の不整合**: `verify_pointer_for_cli` は
  `read_tombstone` (`main.rs` L6923-6938 → `PurgeState::read_tombstone(raw_hash)` の
  `record.is_active()` = 自身の tail が `Purged` かどうかのみ判定) を使うため、erase receipt 側の
  `retired` を一切参照せず、tombstone 自身の tail=`purged` だけを見て `status: "tombstoned"` を
  誤って返す — LC10 が resolver 一般に求める「個別 marker の末尾で短絡しない」という規範に
  `evidence verify` だけが違反する具体的な再現ケース。

### PB65 LC12/13/14 分岐の evidence verify 経由でのエラーコード忠実性 (パラメタ化) [P0]
- 正本: 08 §3.1 手順 5 (ii)(iii)(iv) L192-204（**LC12/LC13/LC14**
  と同一分岐、`main.rs` の `enforce_canonical_marker_barrier` L7069-7085 が `open`/`view`/`restore`
  向けに既に実装済みの 4 分岐)。
- 前提: (a) canonical final event = `erased`・raw 不在 (LC12 相当)。(b) canonical final event =
  `retired`・raw 不在 (LC13 相当、resurrection のはずが raw が無い異常系)。(c) marker 皆無・raw 不在
  (LC14(a) 相当、unmarked corruption)。
- 操作: 各パターンで `kio evidence verify <pointer>` を実行する。
- 期待: (a) `status:"not_found"`, `error_code:"KIO-E-PURGE-NOT-FOUND-001"`。(b)(c) は
  `KIO-E-STORE-CORRUPT-001` (「not_found 相当だが corruption」— LC13/LC14 は not_found と区別される
  独立コード)。**現行実装との既知の不整合**: `verify_pointer_for_cli` は raw 不在時、entry の有無に
  関わらず `not_found_verify_output` (`verify_objects.rs` L209-218) を呼び、**常に**
  `KIO-E-PURGE-NOT-FOUND-001` を返す — (b)(c) のケースで `KIO-E-STORE-CORRUPT-001` を返すべきところを
  誤って `KIO-E-PURGE-NOT-FOUND-001` にしてしまう。`main.rs` の `enforce_canonical_marker_barrier`
  が既に (a)(b)(c) を正しく区別する実装を持つため、本契約は「同じロジックを再実装せず共有せよ」
  という §PB66 の構造契約と対になる。

### PB66 [構造契約] verify_pointer_for_cli は canonical_final_event / enforce_canonical_marker_barrier 相当を呼ぶ (read_tombstone 単独判定の禁止) [P0]
- 正本: 05 §3.5 L907『検証失敗の marker は入口を問わず (fsck・resolver・再 purge) 説明能力を持たない
  corruption とする』の前提となる一般原則 — 08§3.1 手順 5 の 4 分岐が resolver 全般の正本アルゴリズム
  であること (§冒頭の引用)、および `main.rs` L7020-7054 の doc comment 自身が述べる
  「`kio_core::purge::canonical_final_event` フェッド `enforce_canonical_marker_barrier`」という
  設計。
- 前提: PB64/PB65 で確認した挙動差分。
- 操作: `verify_pointer_for_cli` の実装 (コードレビュー水準) を検査する。
- 期待: `verify_pointer_for_cli` は `PurgeState::read_tombstone(...).is_active()` の単一 marker
  判定や `barrier_blocks` (active journal のみを見る、canonical lifecycle とは無関係 —
  `purge.rs` L909-914) を lifecycle 判定の代替として使わず、`kio_core::purge::canonical_final_event`
  (両 marker の tail を集約する関数、`purge.rs` L564-594) または `main.rs` の
  `enforce_canonical_marker_barrier` そのものを呼び出す構造になっていること。`open`/`view`/`restore`
  向けの実装とは別に、`evidence verify` 専用の並行した 4 分岐判定ロジックを新規に書き起こす実装
  (ロジックだけを複製し関数は共有しない実装) は、たとえ PB64/PB65 の入出力契約を個別に満たしても
  本契約 (構造契約) には違反する — 「fsck と resolver で扱いを割らない」(08§3.1 手順 5 L185-187)
  という規範は同一 **実装** の共有を要求する趣旨と読む。

### PB67 [regression-lock] LC21 の malformed marker 一貫性は低レベル parse 共有により既に成立 [P1]
- 正本: 08 §3.1 および現行 Rust tests（**LC21** provenance）『検証失敗の marker は入口非依存で corruption
  (fsck/resolver/re-purge 統一)』
- 前提: 構造検証 (kind 別必須 field 欠落等、LC16 相当) に失敗する tombstone レコードを用意する。
  `verify_pointer_for_cli` も fsck の `canonical_lookup` (`verify_objects.rs` L1517 以降) も、共に
  `PurgeState::read_tombstone`/`read_erase_receipt` (`purge.rs` L923-952、内部で
  `parse_tombstone_bytes`/`record.validate_structure()` を呼ぶ) を経由して読む。
- 操作: (a) `kio repair --verify-objects`、(b) `kio evidence verify <pointer>` (raw が同時に不在の
  ケースを含めない — 純粋に「malformed record が読めるか」のみを見る) をそれぞれ実行する。
- 期待: (a)(b) いずれも同一の `KIO-E-STORE-CORRUPT-001` で終端する — 両者が最終的に同じ低レベル
  parse/validate 関数 (`purge.rs` の `PurgeState::read_tombstone`) を呼んでいるため、LC21 が求める
  「malformed marker の corruption 判定一致」は**現状でも既に成立している可能性が高い**
  ([適合済みの可能性] — regression-lock として固定する)。**PB64-66 との違いに注意**: PB64-66 は
  構造的に**正当な** (validation 通過済みの) marker 同士の canonical 集約の食い違いを扱い、本契約は
  構造的に**不正な** marker の corruption 判定一致のみを扱う — 前者は現行実装で未達、後者は既に
  低レベル共有により達成されている、という非対称を正しく区別する。

### PB68 [統合契約] verify と open/view/restore は同一 fixture で同一 error_code/status を返す (cross-command 一致) [P0]
- 正本: 08 §3.1 手順 5 冒頭『fsck と resolver で扱いを割らない』の一般原則を `evidence verify` と
  `open`/`view`/`restore` という**複数の resolver コマンド間**に適用したもの (08 の resolver 系
  コマンド一覧 — §7.1『kio evidence verify / kio view / kio open / kio evidence retarget』はいずれも
  同じ pointer 解決規範の消費者)。
- 前提: PB64/PB65 の 4 パターン (canonical=purged / erased+raw不在 / retired+raw不在 / marker無し+raw
  不在) をそれぞれ用意する。
- 操作: 同一 fixture に対して `kio evidence verify <pointer>` と `kio open <pointer>` (または
  `kio view <pointer>`) を実行し、返る `error_code` (または `status`) を突合する。
- 期待: 4 パターン全てで両コマンドの `error_code`/`status` が一致する (`tombstoned`↔
  `KIO-E-PURGE-TOMBSTONED-001`, `not_found`↔`KIO-E-PURGE-NOT-FOUND-001`,
  `KIO-E-STORE-CORRUPT-001`↔`KIO-E-STORE-CORRUPT-001` の 2 パターン)。PB66 の構造契約 (同一実装の
  共有) が満たされれば本契約は自動的に成立するはずであり、本契約はその**外部から観測可能な結果**を
  固定する end-to-end regression として位置づける (PB66 が構造を、PB68 が結果を固定する対の関係)。

---

## Y. 解釈割れ注記一覧

1. **§C (PB07) / names.jsonl vs 現行の dual-ref-write 方式**: spec (10§7.5.1 L505-509) は
   `names.jsonl` という append-only な論理名ログを明示するが、現行実装は `refs/tags-v1/tag-<digest64>`
   (canonical) と `refs/tags/<logical_name>` (legacy 表示名) の二重書き込み + 相互一致検査
   (`verify_objects.rs` L1430-1491) という別方式で「digest ↔ 論理名対応」を実現している。前者は
   一方向 hash の逆引きを別ファイルの存在 (`refs/tags/<name>`) に依存する設計であり、後者
   (`names.jsonl`) は明示的な双方向マッピングを持つ設計である。両者が同一の保証 (改名・削除の
   監査可能性、torn tail 耐性) を提供するかどうかは、`refs/tags/` 自体が現状 torn tail や途中
   malformed 行の概念を持たない単一値ファイルの集合であるため、**spec が要求する「全行検証」という
   ログ構造そのものへの移行が必要か、現行の dual-ref 方式で同等の保証を代替できるか**は実装判断を
   要する。本書は names.jsonl 前提で契約を書いたが、この判断自体は §C の担当実装時に確定させる
   必要がある。
2. **§G (PB19) / 将来の schema 変更が既定経路 (rebuild) を守るかの testability**: 10§7.5.3 の
   「既定は rebuild」規約は、未来に書かれるコードの規律についての規範であり、現在のコードベースを
   スナップショットで検査する自動テストは「今のところ違反が無い」ことしか確認できない。CI で
   継続的に強制する仕組み (例: 新規 `ALTER TABLE` 呼び出しの lint) を別途設けるかどうかは本書の
   スコープ外。
3. **§I (PB27) / バックアップ最低保全集合の自動検証手段の欠如**: 専用バックアップコマンドが MVP に
   存在しないため、「truth 区分全行を含める」という規範を実行可能な形で検証する対象コード自体が
   無い。将来 `kio backup` 相当のコマンドが実装された場合に初めて本項目は通常の契約として機能する。
4. **§J (PB28) / 「同じ完了 Tx」の読み方**: 04§5.7/05§3.5 L760-761 の「完了 Tx で現 counter 値に
   初期化する」を、(a) `rebuild_sqlite_index` 自体の SQLite トランザクション単位で読むか、(b)
   `run_repair` 全体が保持する `.kio/.lock` の単位で読むかで、現行実装 (`recover_index_generation`
   を `rebuild_step3_index` 直後に呼ぶ、`main.rs` L970-972) が「充足」か「未達」かの評価が分かれる。
   本書は両読みを併記し、いずれの場合も rebuild と `recover_index_generation` の間の crash 窓での
   自己修復可能性 (次回書込コマンドでの再訪) を別途確認する契約として PB28 を維持した。
5. **§W (PB63) / legacy tree の識別手段が未規定**: 08§2 L50 の「forward 規則制定以前に作られた
   検証済み legacy tree」という条件を、実装が個々の tree entry 検証時にどう判定するか (commit
   created_at の閾値比較か、tree 自体への schema バージョンタグ付けか) を spec 本文が明示しない。
   実装着手前に発注側裁定が必要。
6. **§U (PB60) / 「text alignment 成立」の判定アルゴリズム未規定**: 08§5 L429 は「新旧の normalized
   text 間で text alignment が成立した領域内でのみ」fuzzy 対応を用いると述べるが、具体的な
   alignment アルゴリズム (例: LCS ベース diff の一致率閾値か、editdistance ベースか) を規定しない。
   Phase 4+ 実装時に確定が必要 (現時点では契約の「入出力」レベルの固定にとどめ、アルゴリズム内部を
   先取りしない)。

## Z. 裁定 (§Y の解釈割れ — 実装用、2026-07-22 オーケストレータ裁定)

1. **PB07**: **names.jsonl 方式へ移行する** — spec は台帳構造 (append-only・torn tail・全行検証) まで明文規定しており、fsck 契約 (U41) がその検証を要求する。既存の refs/tags/<logical_name> は legacy 読取のみ残し、新規 tag 作成は canonical ref + names.jsonl 追記の対で行う。
2. **PB19**: 現状違反なしの snapshot 契約で足りる。ALTER TABLE lint 等の CI 強制は Phase 4 の実装フィードバックへ記録。
3. **PB27**: 契約は文書整合の確認に留める (バックアップコマンドは MVP 外 — 将来実装時に有効化)。
4. **PB28**: **(a) 同一 SQLite Tx を正とする** — rebuild 書込と index_generation 採番 + last_lifecycle_epoch 初期化が原子でないと crash 窓で「新 index × 旧 generation」が残るため。既存の次回書込時自己修復は defense in depth として維持。
5. **PB63**: legacy tree の識別は**検証時に path が forward 拒否集合に該当するかの事実ベース** — tree 版タグ・created_at 閾値は導入しない。該当 entry は legacy 警告 + 読取可・物理化は対象 OS 検査で拒否 (U119 の枠)。
6. **PB60**: 入出力契約のみ固定し、alignment アルゴリズム内部は Phase 4+ で確定 (作成者方針を承認)。

---

**契約数集計**: A=3, B=3, C=3, D=2, E=6, F=1, G=2, H=6, I=1, J=4, K=2, L=3, M=2, N=3, O=3, P=3, Q=3,
R=2, S=5, T=1, U=3, V=1, W=1, X=5 — 合計 **68 件**
(P0=45, P1=19, P2=4)。解釈割れ注記 = **6 件** (§Y)。
