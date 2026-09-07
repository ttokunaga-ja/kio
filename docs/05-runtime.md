# 05 Runtime

統合元: 旧 `research/hybrid.md` (検索モード) + 旧 `research/commit_snapshot.md` (commit_type, GC, purge) + 一部旧 `research/read_only.md` (検索結果での書き込み境界) + 一部旧 `research/productization_notes.md` (運用)。いずれも正本ではなく、2026-07-18 に docs から撤去 (経緯は git 履歴で参照可)。

---

# 1. 検索

## 1.1 モード

```
text   FTS5 (BM25)         常に利用可能
image  候補を画像に限定      embedding 互換性あり時に利用可能 (2026-08-11 追加)
vector sqlite-vec          embedding 互換性あり時に利用可能
hybrid RRF(text, vector)   両方利用可能時のみ。auto モードがデフォルト
```

**画像に順位が付くのは、そのモードがベクトルレーンを持つときに限る** (2026-08-11 確定)。
政策ではなく**能力の制約**である — `text` にはベクトルレーンが無く、
画像を評価する手段が存在しない。

| mode | 画像に順位が付くか |
|---|---|
| `text` | **不可** |
| `image` | 画像のみ |
| `vector` / `hybrid` | **可** (同一マルチモーダル空間 — [07-adapter-spec.md §5.3](07-adapter-spec.md)) |
| `auto` | hybrid に解決したときは可 / text fallback 時は不可 |

`--mode image` は**候補クラスを画像に限定する軸**であって、リンク先を選ぶ軸ではない。
**単体の画像ファイル**がヒットした行はその画像を指すが、**文書中の図**がヒットした行は
引用元 chunk へ解決して**文書を指す** (裸の画像行を返さない)。したがって
`--mode image` の結果には画像名と文書名が混在しうる。

`.kio/config.toml`:

```toml
[search]
default_mode = "auto"            # "auto" | "text" | "image" | "vector" | "hybrid"
fail_behavior = "fallback"       # "fallback" | "error" | "warn"
```

`auto` の解決順:

```
--offline 指定 かつ embedding Adapter が online_api → text fallback (fallback_reason="offline" — 送信自体を行わない短絡。下記)
embedding profile_hash 不一致 → text fallback (KIO-E-SEARCH-VEC-INCOMPAT-001)
embedding 承認なし (下記 consent gate。online_api のみ) → text fallback (KIO-E-SEARCH-VEC-UNAUTHORIZED-001)
同一 query が in-flight ([04-pipeline.md §5.4](04-pipeline.md)) → text fallback (fallback_reason="embedding_in_flight")
query embedding 応答が受入検査 ([07-adapter-spec.md §5.3](07-adapter-spec.md)) で contract violation → text fallback (fallback_reason="embedding_contract_violation")
上記のいずれにも該当せず vector のみ利用不能 (index 未構築等の技術的理由) → text
両方利用可能 → hybrid
両方不可 → error (KIO-E-SEARCH-VEC-UNAVAIL-001)
```

解決順の列挙は**判定順序**でもある — 複数条件が同時に成立する場合は先に列挙された行の
`fallback_reason` / error code を採用する (profile 不一致 (INCOMPAT) が承認なし (UNAUTHORIZED) に先行)。
`fail_behavior = "warn"` の挙動は **fallback と同じ結果** (text fallback + `fallback_reason`) に加えて
構造化 warning を stderr / `--json` の `warnings[]` へ出す — exit code も fallback と同じ (error に
しない)。

**query embedding の consent gate**: vector | hybrid の page 1 は query embedding (07 §5.3 の
`input_type: "query"` — sync 呼出) を要する。**採用中の embedding Adapter の `execution_mode` が
`online_api` の場合に限り**、これは新規送信として [07-adapter-spec.md §3](07-adapter-spec.md)
の opt-in gate の対象である (payload は query 文字列のみで folder 内容を含まない)。**送信可否 =
参加 scope の 1 つ以上に当該 embedding Adapter の active な `approvals[]` 行があり、かつ当該 scope の
実効 `allow_network` が true であること** (未設定・設定 key の喪失は gate 不成立 —
[07-adapter-spec.md §3](07-adapter-spec.md) と同一規範。`--online` が開くのは未設定の既定閉鎖のみ —
明示 revoke (`allow_network = false`・行の revoked) は上書きしない)。**この可否は
source cost ledger を開く・stale sweep / claim を開始する**前**に approvals[] / boolean を再読して最終検証する**
(読み取り開始時の値を使い回さない。ここで revoke が先行した auto / hybrid は ledger を開かず text fallback になる。
検証後に revoke が完了した場合の当該送信は in-flight として許容し、以後の claim / reservation / send / settlement は
fresh page 1 の通常の記帳経路に従う — 送信済みの取り消し非保証 — [07-adapter-spec.md §3](07-adapter-spec.md)))。承認ゼロ
(かつ `--online` 一時 opt-in なし) の場合、auto / `--mode hybrid` は text fallback
(`fallback_reason="embedding_not_authorized"`)、`--mode vector` 明示は KIO-E-SEARCH-VEC-UNAUTHORIZED-001
で error。**ユーザー意思由来の text fallback は `fail_behavior` の対象外である** — `fail_behavior` は技術的
失敗 (INCOMPAT / UNAVAIL 等) への応答方針であり、`embedding_not_authorized` (承認なし) と `offline`
(`--offline` 指定) には適用しない (設定値に関わらず auto / `--mode hybrid` は常に text fallback、`--mode vector`
のみ error — §1.2 / [06-cli-spec.md §3](06-cli-spec.md) の `--mode hybrid` 行の注記も同旨)。一方
**`embedding_in_flight` (同一 query の並行実行 — [04-pipeline.md §5.4](04-pipeline.md)) と
`embedding_contract_violation` (query embedding 応答の受入検査違反 — [07-adapter-spec.md §5.3](07-adapter-spec.md))
は技術的な過渡失敗であり `fail_behavior` の対象**: auto は text fallback、`--mode hybrid` は fail_behavior に従い、
`--mode vector` 明示は KIO-E-SEARCH-VEC-UNAVAIL-001 で error。`--online` / `--offline` は他コマンドと
同義の当該実行限りの上書き (07 §3)。**`--offline` 指定時は承認の有無に関わらず query embedding を送信
しない** — auto / `--mode hybrid` は text fallback (`fallback_reason="offline"`)、`--mode vector` 明示は
KIO-E-SEARCH-VEC-UNAVAIL-001 で error。課金は
`scope_id='device'` の sync request として縮退 2 相に記帳する ([04-pipeline.md §5.4](04-pipeline.md)
— folder cap 対象外・device cap / per_adapter は通常合算)。

> **`offline_api` は本 gate の対象外 (2026-07-26 確定)**: ローカル embedding server
> ([07-adapter-spec.md §3](07-adapter-spec.md) — url は loopback リテラルに限定される) は
> 送信を行わないため、**送信を gate する本節の機構には適用対象が無い**。
> `approvals[]` 行・`allow_network` boolean のいずれも要求せず、上の解決順の
> `embedding_not_authorized` / `offline` にも該当しない (どちらも「送信の可否」に
> 由来する縮退であり、送信が存在しない経路には生じ得ない)。したがって
> **`--offline` 指定下でも local embedding による vector / hybrid 検索は成立する**
> (上の「`--offline` 指定時は承認の有無に関わらず query embedding を送信しない」は
> online_api Adapter についての規範である — 送信しない Adapter に禁止する送信は無い)。
> `--online` も同様に無関係である (開くべき閉鎖が存在しない)。
> 課金はローカル単価 0 として記帳されるため device cap の扱いも変わらない。
>
> **本節の残りの規範は execution_mode に依らず適用する** — profile_hash 不一致
> (INCOMPAT)・`embedding_in_flight`・`embedding_contract_violation`・
> [07-adapter-spec.md §5.3](07-adapter-spec.md) の受入検査はいずれも「送信してよいか」
> ではなく **vector が正当か**を問う規範であり、ローカル生成のベクトルにも等しく効く。

## 1.2 CLI

```bash
kio search "..."             # auto
kio search "..." --mode text    # text only
kio search "..." --mode vector  # vector only。失敗時は error
kio search "..." --mode hybrid  # hybrid 強制。vector 失敗時は fail_behavior に従う (承認なし・--offline は対象外 — 常に text fallback。embedding_in_flight は対象 — §1.1)
kio search "..." [--online|--offline]  # query embedding の一時 opt-in / 当該実行の新規送信禁止 (§1.1 consent gate)
```

## 1.3 RRF (Reciprocal Rank Fusion)

候補取得: text / vector 各バックエンドから検索対象集合 (§1.6) 内の上位 `candidate_depth` 件 (デフォルト
200) を **unique semantic chunk (`scope_id,chunk_hash`) 単位**で取得し、和集合を候補プールとする。
default / `--at` / `--include-deleted` の結果上限は候補プール件数。`--all-history` / `--since` は
MMR/dedup 後に historical aliases を展開するため、最終 hit 数は retained semantic chunks の distinct
`(chunk_hash,path)` binding 数 (history-walk aggregate cap 内) まで増えうる。

```text
RRF_score(c) = w_text / (k + rank_text(c)) + w_vector / (k + rank_vector(c))
default: k = 60, w_text = 1.0, w_vector = 1.0
```

- `rank_*` は各バックエンド内の 1 始まり順位。バックエンド内の同点は chunk_id 昇順で順位を確定する
- **短語 fallback**: query の全 token が 3 文字未満で trigram tokenizer の MATCH が成立しない場合
  (例: 1〜2 文字の日本語 query — MATCH は 0 件になる)、text バックエンドは `chunks.text` への
  **bounded LIKE スキャン** (上限 = `candidate_depth`、instr ベースの部分一致) へ fallback する。
  3 文字以上の unit が 1 つでもあれば FTS MATCH を使う — **MATCH 式に渡すのは 3 文字以上の
  unit のみ**とし、**3 文字未満の unit は混在 query では候補確定に用いない (drop —
  2026-07-22 実装フィードバック #2)**: trigram は 3 文字未満の phrase を黙って落とすため MATCH には
  載らず、旧規範 (「同一 bounded query 内の `instr` 条件として LIMIT 前に AND 適用」) は自然文
  query の助詞 (「が」「の」) や英語機能語 (`in` `to`) を全候補への hard filter に変え、それらを
  含まない簡潔な文体の本命 chunk を構造的に排除した (eval M3-2/M3-3 実測 — 本命が bm25 首位でも
  候補集合から消える)。混在 query の短語は bm25 に寄与しない stopword として扱う。
  **全 unit が 3 文字未満の場合のみ**、従来どおり短語 instr 条件を text / vector 両バックエンド
  共通の eligibility 述語として候補確定 (candidate_depth 充足前) に AND 適用する — 和集合・RRF に
  短語欠落候補を入れない。vector 側の適用形 (pure-short 時): `chunk_vec` を
  `chunks` へ JOIN して instr 述語を適用した母集合に対し distance 順で LIMIT candidate_depth を確定する
  (brute-force KNN — [10-operations.md §6](10-operations.md)。vec0 の `k =` 構文等、述語適用**前**に
  内部 top-k を確定させる形は用いない — 述語後の候補が痩せて candidate_depth を満たせなくなるため)。
  LIKE fallback の順位も決定的に定める:
  最初の一致位置 (instr) 昇順、同点は chunk_id 昇順。SQL は ORDER BY 確定後に LIMIT candidate_depth
  を適用する (LIMIT 先行で候補集合が非決定になる形は禁止)
- **MATCH 式の生成**: user query を FTS5 構文として解釈しない — token 列を各々二重引用符で囲んだ
  phrase / term の並びとして MATCH 式を機械生成する (token 内の `"` は `""` へ escape。`C++` 等の
  記号語が fts5 syntax error にならない)。FTS5 演算子 (AND / OR / NEAR / `*` 等) の直接指定は
  MVP では提供しない。**tokenization は決定的に固定する**: NFC 正規化後の query を Unicode 空白で
  分割した各非空片が token (長さの単位 = Unicode scalar 数。記号のみの token も phrase として投入可)。
  token が 0 個の query は KIO-E-CONFIG-USAGE-001 (exit 2)。
  **決定的スクリプト境界分割 (tokenization の後段・2026-07-22 実装フィードバック #2)**: 空白分割で
  得た各 token をさらに Unicode スクリプト境界で決定的に細分し、**元 token と細分片の両方**を
  MATCH 生成の単位 (unit) とする — 元 token の phrase は正確な連接一致の信号として保持し、細分片が
  膠着形 (query「スコープが」 vs 本文「スコープは」) や記号連結 (query `read/write/admin` vs 本文
  `read / write / admin`) の表記ゆれを吸収する。細分規則 (実装リリースに固定):
  (1) 文字クラス = ひらがな / カタカナ (U+30FC 長音は直前のかな run に付随。**中点 ・ U+30FB は
  カタカナブロック内だが script=Common のため「その他」= separator とする — 2026-07-22 #2 補正**:
  これが無いと「アクセス・トークン」が 1 unit に固着し細分が効かない) / 漢字 (々・〆 含む) /
  英数 run (`[0-9A-Za-z]`。数字に挟まれた `.` `,` は run 内に保持 — `99.9` / `3,600` / `3.2GB` は
  1 unit) / その他 (記号等)。(2) クラス遷移点で分割し、「その他」クラスの片は separator として
  unit にしない (元 token 側の phrase には残るため `C++` 等の記号語の一致性は失わない)。
  (3) 細分の結果が元 token と同一なら重複させず、全 unit 集合から重複を除く。細分は入力 token の
  文字列だけから固定規則で決まる — query 由来性はフィードバック #1 と同一の原理。3 文字以上の
  unit が phrase として MATCH 式に入り、3 文字未満の unit は上記「短語 fallback」の規則 (混在
  query では drop) に従う。この細分と短語 drop が無いと、自然文 query の膠着 token・機能語が
  exact phrase / hard filter として働き [09-mvp-scope.md §4.3](09-mvp-scope.md) の Recall 目標を
  構造的に割る (eval M3-2/M3-3 実測で失敗 14 件全てがこの 2 構造に帰着)
  **決定的 query 正規化 (MATCH 生成の前段・2026-07-22 実装フィードバック #1)**: 生成に先立ち、
  各 token (フィードバック #2 の細分後は各 unit) に決定的な同値展開を適用してよい — (1) 4 桁以上の純数値 token の桁区切り同値形
  (`3600` ↔ `3,600`)、(2) Kio 同梱の固定対訳辞書 (実装リリースに固定・実行時変更不可) による
  用語の対訳形。展開結果は当該 token と同値 phrase の OR 並置として投入する。
  「query 由来でない追加語」の禁止が指すのは入力に由来しない語 (推測・履歴・文脈からの注入) で
  あり、**入力 token から固定規則で決定的に導出される同値形は query 由来である** — この展開が
  無いと数値・対訳表記の不一致が [09-mvp-scope.md §4.3](09-mvp-scope.md) の Recall 目標を
  構造的に割る (eval M3-2/M3-3 実測で確認)
- 片方のバックエンドにしか現れない候補は、現れない側の項を 0 とする
- `RRF_score` の同点は chunk_id 昇順
- text-only / vector-only モードでは fusion せず当該バックエンドの順位をそのまま使う
- 実装規則: `candidate_depth` の上限は rank 計算 (window 関数等) の**入力になる内側段 (サブクエリ)** で
  効かせる。外側の LIMIT では全マッチ行が rank 計算の入力に入り、大ヒット数クエリで実行コストが
  数十倍に膨張する (出典: 旧 `research/folder-history-sqlite-design.md` §18 の実測 — VM step 1,074 → 70,374。2026-07-18 撤去、git 履歴で参照可)。
  **有界エスカレーション (2026-07-22 実装フィードバック #3)**: 内側段は eligibility (公開・config
  世代・時点条件) を含まないため、非適格行が bm25 上位を占めると適格候補が内側 LIMIT で飢餓し得る
  (L83-84 の「検索対象集合内の上位 candidate_depth 件」との緊張)。適格候補が `candidate_depth` に
  満たない場合に限り、内側 LIMIT を 4 倍して最大 2 回まで決定的に再実行してよい (それでも不足なら
  得られた分を候補とする — コスト上限は初回の高々 21 倍で有界、通常経路は 1 回のまま)。この
  エスカレーションは候補「集合」の充足のみを変え、順位規則 (bm25 → chunk_id) は不変

```toml
[search.rrf]
k = 60
w_text = 1.0
w_vector = 1.0
candidate_depth = 200
```

## 1.4 多様化 (MMR / Dedup)

素の RRF だけでは同一原文の隣接 chunk が上位を独占しやすいので、後処理で多様化する。

```toml
[search.diversify]
enabled = true
strategy = "mmr"            # "mmr" | "group_by_raw_hash" | "off"
mmr_lambda = 0.7            # 1.0=relevance only, 0.0=diversity only
max_per_raw_hash = 3
```

MMR 選択則:

```
score(c) = λ * relevance(c) - (1-λ) * max_{c' ∈ selected} similarity(c, c')
similarity = embedding の vector cosine (これのみ。2026-07-03 確定 — embedding が無い場合は
             MMR 自体を適用しないため、代替 similarity は定義しない)
selected = ∅ の初手は similarity 項を 0 とする (= relevance 最高の候補を既定 tie-break 順で
             選ぶ — 実装間で初手が揺れない)
```

適用範囲と決定性:

- MMR は候補プールの RRF 上位 `mmr_depth` 件 (デフォルト 100、`candidate_depth` 以下) に対して **1 回だけ** 適用し、並べ替え済みの**確定順序**を得る。`mmr_depth` 以降の候補は RRF 順のまま末尾に接続する
- `relevance(c)` = RRF スコアを **MMR 候補プール内で min-max 正規化した値** ([0,1]。全候補が同スコアなら一律 1.0。2026-07-03 確定、step3a §C の決定性論点解消 — 生の RRF スコア (最大 ~1/k) をそのまま使うと mmr_lambda の意味が損なわれるため)。`similarity` は embedding の cosine。embedding が無い場合 (text-only 検索) は MMR を適用せず RRF 順のままとする (ただし `max_per_raw_hash` の dedup は embedding 非依存であり text-only でも適用する)。**hybrid の候補プールに embedding 未付与、または profile 非互換で cosine を計算できない候補が 1 件でも混在する場合 (部分 enrichment / §1.8 の profile 不一致 text fallback を含む) も MMR は適用しない** — pairwise similarity が全対で計算できないため。dedup のみ適用し RRF 順で返す。MMR score の同点は RRF 順、さらに同点は immutable `(scope_id,chunk_hash)` の UTF-8 byte order
- `max_per_raw_hash` は alias 展開**前**の unique semantic chunk stream に適用する (ページを跨いで
  raw_hash あたり最大 N semantic chunks)。retained chunk の historical path aliases は provenance 行で
  あり、この上限へ再カウントせず全件を返す

> **image 行の扱い (2026-07-26 確定)**: [04-pipeline.md §4.3](04-pipeline.md) の `image_vec`
> 新設により、候補プールには chunk 行に加えて **image 行** (`result_type: "image"` — §1.7) が
> 混在し得る。本節の規則はいずれも**型で分岐させない**。
>
> - **`max_per_raw_hash` は image 行も同じ枠を消費する。** 画像専用の quota も lane も
>   設けない。本節冒頭が述べる cap の目的は「同一原文が上位を独占しない」ことであり、
>   その原文から出た結果が chunk か image かは Agent から見た占有度を変えないためである。
>   カウント先の raw_hash は当該 result 行の `evidence_pointer.raw_hash`
>   (= §1.7 が定める参照元 chunk のもの) — **1 result 行 = 1 evidence_pointer = 1 raw_hash**
>   で一貫させる。
> - **MMR の無効化条件は型に依らない。** 上の「候補が 1 件でも混在する場合」は
>   chunk・image のどちらにも等しく適用する。「image は必ず embedding を持つ」という
>   前提は置かない — 画像埋め込みも chunk と同じ Batch / budget 機構に載るため、
>   同じく部分 enrichment 状態 (§1.7 の `enriched_ratio`) を取り得る。
> - **MMR の tie-break key `(scope_id, chunk_hash)` はそのまま使える。** §1.7 のとおり
>   image 行も参照元 chunk の `evidence_pointer` を持つため `chunk_hash` が定まる。
>   新しいキーを導入しない。
> - **pairwise similarity に特別扱いは要らない。** [03-data-model.md §7](03-data-model.md) と
>   [07-adapter-spec.md §5.3](07-adapter-spec.md) が単一マルチモーダル空間を強制するため、
>   image と chunk の vector は定義上そのまま cosine 比較できる
>   ([04-pipeline.md §4.3](04-pipeline.md) — `chunk_vec` / `image_vec` の物理分割は
>   sqlite-vec の制約であって意味的分離ではない)。
- 入力 (chunk 集合・query・設定) が同じなら確定順序は常に同一 (決定論)。これがページング (§1.5) の前提

```toml
[search.diversify]
# (既存キーに追加)
mmr_depth = 100
```

## 1.5 ページング / カーソル

```bash
kio search "..." --limit 20
kio search "..." --limit 20 --offset 20         # 同一 snapshot 内
kio search "..." --limit 20 --cursor <token>    # snapshot 越し安全
```

ページングは「確定順序 (§1.4) の決定論的再計算」で実現する。cursor に MMR の selected 集合や score は持たない。レスポンスに `next_cursor` を含める。本節の定義は単一 scope 内の sub-cursor であり、複数 scope 横断時の cursor 全体構造 (opaque token、`scope_mode` / `query_hash`) は §1.8 で定義する。

scope ごとの sub-cursor は
`{scope_id, snapshot_commit, index_generation, max_rowid, max_association_rowid, chunking_config_hash, consumed}`。
`index_generation` は **rebuild (`kio repair rebuild-db`)・purge・embedding enrichment の finalize・
index / batch finalize で `chunk_fts` の内容が変化した場合・tombstone lifecycle の更新
(retire・再 purge — tombstone 状態の判定 (canonical final event — [08-evidence-pointer-spec.md §3.1](08-evidence-pointer-spec.md) 手順 5) が検索の可視集合を変えるため、purge の回転と対称。
[§3.5](05-runtime.md))・および GC の shallow 化実行
(`--all-history` cursor の walk 対象が変わる) の、いずれでも新規採番する ULID**
(単調カウンタではない — sqlite.db の `index_metadata` 表 ([04-pipeline.md §4.1](04-pipeline.md)) に保持するため
DB 喪失で数が戻っても、ULID なら旧 cursor が偶然一致して誤受理されることがない。FTS 内容変化でも回転する
理由: FTS5 の bm25() は文書頻度・平均長という**大域統計**を使うため、cursor が chunk 集合を rowid 上限で
固定しても、後発行の追加で既存行の順位自体が変わり得る — 誤った続きを返すより旧 cursor を拒否する)。**回転はそれを引き起こした SQLite 書込 (FTS 内容を変える INSERT / UPDATE / DELETE、purge の行削除等) と同一の SQLite Tx で行う** — 別 Tx にすると、間の crash で旧 cursor が変化後の stream に受理される (file 側の tombstone lifecycle 更新に伴う回転だけは同一 Tx にできないため、§3.5 の lifecycle-epoch カウンタ + 補完規則で crash 窓を閉じる)。**replay 時に現在値と不一致なら
`KIO-E-SEARCH-CURSOR-001` で拒否する** (再検索が正) — rebuild は rowid を再採番し、purge は
append-only 前提を破って行を削除し、後発 embedding は hybrid の候補集合・順位を変えるため、
いずれも旧 cursor の `max_rowid` / `consumed` の意味を失わせる。
token 全体には canonical `time_travel` selector を、`--since` ではさらに
page 1 の `since_cutoff` (UTC ISO8601 + `Z`) も保持する:

- `snapshot_commit`: 当該 scope の検索対象 commit (§1.7 snapshot_at)。2 ページ目以降も同じ commit の tree_entries ([04-pipeline.md §4.5](04-pipeline.md)) で絞る
- `max_rowid`: cursor 発行時点の chunks 最大 rowid。`--all-history` / `--include-deleted` では `rowid <= max_rowid` で chunk 集合を固定する (chunks 行は append-only ([04-pipeline.md §4.1](04-pipeline.md)) なので単調増加)
- `max_association_rowid`: cursor 発行時点の `chunk_config_generations` 最大 association rowid。
  現行 config association も `association_rowid <= max_association_rowid` に固定し、page 1 後に追加された
  association が page 2 の候補へ混入することを防ぐ
- `chunking_config_hash`: page 1 で検索対象にした tree の config (デフォルト = **当該 scope の HEAD tree の値** (移行期間の扱いは [04-pipeline.md §4.6](04-pipeline.md))、時点指定 = 対象 tree の値 — §1.6)。replay 時の対象値と不一致なら拒否する
- `consumed`: alias expansion 後の final result stream で当該 scope から既に返した hit 数 (semantic chunk
  数ではない)。replay は grouped final stream を完全再計算し、scope ごとにこの件数だけ先頭 hit を skip
  するため、page boundary が 1 chunk の alias group 内でも重複/欠落しない
- `since_cutoff`: `--since` の page 1 で一度だけ計算した下限。page 2 以降は現在時刻から再計算しない
- `query_hash` (token 全体に 1 つ、§1.8) が不一致の cursor は `KIO-E-SEARCH-CURSOR-001` で拒否する

2 ページ目以降は同一の候補取得 → RRF (§1.3) → MMR (§1.4) を再計算し、consumed 件を skip して続きを返す。**vector / hybrid の replay は page 1 の query vector を再利用する** — query の再 embedding は行わない (provider の非決定性で候補・順位が変わり、consumed の skip が重複・欠落を生む)。page 1 の正規化済み query vector は device-local の `${XDG_CACHE_HOME:-$HOME/.cache}/kio/search-query-cache/<query_vector_digest>` に best-effort で保存する（query 本文は保存しない）。digest (= `query_vector_digest`) は **token の独立 field であり、かつ §1.8 の query_hash 構成要素**である (query_hash は一方向 hash であり、replay が読む cache leaf の鍵は token field 側から得る。vector|hybrid のみ — text mode では field 省略)。replay はこの device cache **だけ**から vector を読み、canonical vector bytes の sha256 を digest と再照合する。cache が無い・壊れている・digest 不一致なら `KIO-E-SEARCH-CURSOR-001` (再検索が正) とし、再 embedding も source `.kio/index/sqlite.db` の `embeddings` / `chunk_vec`、CAS の embedding object も読取り・書込みもしない。replay が読む query vector の唯一の入力は上記 device-local cache である。順序安定性の根拠は SQLite WAL のスナップショット分離**ではなく**、「commit 単位で固定された chunk 集合 + 決定論的な順位計算 + `index_generation` による FTS 内容不変の保証」である。CLI 呼び出しを跨いでも成立する。

`--offset` は cursor の糖衣であり、同じ再現規則で確定順序の `offset` 位置から `limit` 件を返す。**vector|hybrid の `--offset` は単一実行内の slice である** (当該実行が取得した query vector に対する確定順序 — CLI 呼び出しを跨ぐ継続は cursor が正。再 embedding の非決定性は cursor の digest 再利用でのみ回避される)。終端判定は **alias 展開後の final result stream の末尾** — それを超えたら `next_cursor: null` (`--all-history` / `--since` で候補プール末尾を終端にすると最後の alias group を取り残す。default 系は候補プール = final stream で同値)。

## 1.6 Snapshot 越し検索 (`--at`)

```
--at <commit>           指定 commit 時点で indexed だった chunks のみ対象
--at <commit> --vector  指定時点の embedding profile が現在と互換ならOK、
                        非互換なら KIO-E-SEARCH-VEC-INCOMPAT-001
                        (--vector 明示時は fail_behavior に依らず error — §1.2 と同じ。
                         text への fallback は auto / --hybrid のみ)
--all-history           全 commit を横断 (削除済み・移動済み含む)
--include-deleted       現在 working tree に存在しないファイルも対象
--since <duration>      `--since 7d` のように期間指定
```

各モードの検索対象 chunk 集合 (実装規範。schema は [04-pipeline.md §4](04-pipeline.md)):

```text
デフォルト          chunks ⨝ tree_entries(HEAD)     on (raw_hash, tool_profile_hash, gen)
--at <commit>       chunks ⨝ tree_entries(<commit>) on (raw_hash, tool_profile_hash, gen)
--include-deleted   デフォルト集合 ∪ page-1 snapshot tree に存在しない各 logical path について、
                    snapshot の first-parent ancestry でその path を含む newest commit の
                    exact (raw_hash, tool_profile_hash, gen) binding
--all-history       page 1 snapshot HEAD から全 parent edge で到達可能な全 commit の tree binding
                    に現れる chunk 行
--since <duration>  --all-history 集合を chunks.created_at >= now - <duration> で絞る
```

上表は selector の**論理的な集合規則**であり、`kio search` が source SQLite で実行する SQL ではない。writer / repair は scope resolver の答えを `agg_bindings` に射影する。検索時は履歴 selector と cursor replay だけが source **CAS** から exact binding relation (`raw_hash` / profile / gen / `path_at_commit` / `pointer_commit` / current paths / live) を再解決し、その relation を runtime eligibility filter として `agg_bindings` に交差させる。filter は replica 内の FTS / vector / image 候補 SQL に渡され、`candidate_depth` の前に適用される。これは selector / shallow 確認だけの control-plane read であり、候補の本文・vector・image・Evidence metadata、rank、materialize はすべて既に `aggregator.sqlite` に射影済みの行だけを使う。source `.kio/index/sqlite.db` はこの過程で開かず、CAS の答えから候補データを補充しない。必要な projection marker / binding が無ければ source fallback ではなく fail-closed する。

共通フィルタ: `chunk_config_generations` に**対象 tree の `chunking_config_hash`** の association がある chunk のみ
(デフォルト = HEAD tree = 現行値。`--at` は対象 tree の値、`--all-history` / `--include-deleted` は各 binding
tree の値で判定する。全 tree が `chunking_config_hash` を持つため代用経路は無い — [04-pipeline.md §4.1, §4.6](04-pipeline.md))。

**HEAD 不在 (初回 auto snapshot 前・snapshot finalize 未完) の scope は index 未完了として扱う** — 検索は当該 scope を `KIO-E-INDEX-REBUILDING-001` で excluded_scopes に計上し (単独 scope なら exit 3)、cursor は発行しない。**SQLite に反映済みでも未公開 (commit / ref 未 publish) の行は返さない** (§8.1 の finalize 耐久順序の crash 窓で、未公開 snapshot の内容を検索に見せない)。この扱いは**bare (--at なし) の現在状態検索など HEAD 依存の解決経路に限る** — 明示 commit・Evidence Pointer 指定の読取・検証 (単一 scope の search `--at <commit>` を含む) は HEAD 非依存に解決する ([08-evidence-pointer-spec.md §3.1](08-evidence-pointer-spec.md)、[06-cli-spec.md §7](06-cli-spec.md))。
purge 済み raw_hash の chunk 行は物理削除済みのため自然に除外される。
**実装規範**: publication / association の時点条件は correlated **EXISTS** (ancestry 判定と
`association_rowid <= cursor.max_association_rowid` を副問い合わせ内に含む) で評価する — 同一
(chunk_id, config) の複数 introduction 行を素の JOIN で結合すると同一 chunk が重複 hit し、
candidate / rank / cursor を歪める。候補集合は ranking 前に (scope_id, chunk_id) で一意にする。

tree entry の `normalize` が省略された commit では、その entry に eligible chunk は 0 件。`--at` /
history projection / include-deleted のいずれも later `latest_normalize_ref` を補完せず、SQLite cached row が
あっても CAS tree の省略を上書きしない ([03-data-model.md §8])。

- `--include-deleted` が加えるのは page-1 snapshot に path が存在しないファイルの**最終版**のみ。
  snapshot HEAD の first-parent を newest-first に辿り、その path を初めて含む tree entry の persisted
  normalize ref を使う。manifest / `files[status=deleted]` は acceleration/cache に限り、page 1 後の
  mutable manifest 変更は cursor の集合を変えない。途中版まで遡るのは `--all-history` の役割
- `--include-deleted` で同じ semantic chunk に snapshot-live binding が 1 件以上あれば live が勝ち、
  その chunk の旧 deleted-path alias は返さない (rename aliases は `--all-history`)。live twins の
  `path_at_commit` は UTF-8 byte order 最小を使う。live binding が 0 件なら、同じ chunk に対応する
  distinct final-deleted `(path,binding_commit)` を §1.7 の post-ranking group expansion で全件返す
- `--all-history` / `--since` の「全 commit」は page 1 の `snapshot_commit` から全 parent edge で
   到達可能な commit に限る。orphan / disconnected tag-only commit は `--at` で明示する。visited set で
   全 parent を辿り、side parent にだけ存在して merge 結果から消えた binding も対象にする。
   **`--all-history` / `--since` の all-parent walk と `--include-deleted` の first-parent walk 中にある
   shallow 化済み ancestor (tree 破棄済み — §2.2) は skip し、レスポンスに `shallow_skipped` 件数を
   可視化して partial (exit 3) とする**。`--include-deleted` を含め、この partial は all-or-nothing
   ではない。shallow でない tree から解決済みの live / final-deleted binding は候補・結果に残し、
   当該 shallow ancestor にしかない alias だけを除外する。削除済み alias を推測・補完せず、黙って
   欠落させない
- chunk 行が検索対象になるのは auto snapshot (§8.1 — `kio index` / batch finalize の成功完了時) 作成後。indexing 途中の chunk はどのモードでも返さない。auto snapshot は新規 association の creation を fsync してから、**`chunk_publications` と chunks.jsonl の tagged publication event に `(chunk_id, chunking_config_hash, introduction_commit = 当該 commit)` を追記する**。同じ chunk の別 config association は、その config 自身の event を持つまで非適格である。event のない creation/config scalar は検索・履歴 root の authority ではない。
- **時点条件 (正式化)**: デフォルト / `--at` の対象は、上記 join に加えて **対象 `(chunk_id, chunking_config_hash)` の `chunk_publications` のいずれかの `introduction_commit` が対象 commit の ancestor-or-equal である chunk に限る**。判定は publication relation のみを参照し、fallback はない。relation 自体は SQLite cache であり JSONL publication events + commit DAG + tree から決定的に再導出できる ([04-pipeline.md §4.1](04-pipeline.md))。event は introduction commit の tree が同一 `chunking_config_hash` を束縛し、exact pinned normalized unit を認証できる場合だけ有効である。`chunk_config_generations` は pair の creation/order metadata であり、時点性を持たない。この認証は incomparable な複数 introduction を統合せず、それぞれを独立に保持する。**`--include-deleted` の補完 binding にも同条件を適用する** (introduction が当該 binding commit の ancestor-or-equal であること — 削除後に完了した後着 chunk の遡及混入を排除)。**`--all-history` は binding ごとに同判定を行う**
- shallow 化済みの選択 target への `--at`（cursor replay が固定した selected snapshot を含む）は
  `KIO-E-COMMIT-SHALLOW-001` で hard-fail する (§2.2)。上の ancestor skip による partial とは異なり、
  古い replica binding で target tree を代替してはならない

History walk の aggregate security bound は exact に次とする (per-object caps に加算):

```text
all-parent DAG walk:   100,000 unique commits / 10,000,000 total tree entries /
                       4 GiB verified commit+tree bytes
first-parent walk:     100,000 commits / 10,000,000 total tree entries /
                       4 GiB verified commit+tree bytes
```

各 walk は counters を独立に持ち、次の object/entry で 1 つでも超える前に停止する。
`--all-history` / `--since` の scope は candidate/alias を部分返却せず
`KIO-E-COMMIT-HISTORY-LIMIT-001` (`excluded_scopes[].reason=history_limit_exceeded`) で失敗し、既存の
multi-scope partial 規則に従う (部分 = exit 3、全 scope 失敗 = §1.8 の昇格・retryability 分割 —
同一 code 全滅は当該 code の単独時 exit、混在は retryable 理由を含めば 3・全て permanent なら 4)。purge-by-path は all-parent cap、restore-by-path は
first-parent cap を同じ error code で fail-before-mutation/publication する。raw-hash purge と explicit
commit/evidence restore は ancestry walk を必要としない。

過去 snapshot の embedding 再生成は別操作 (`kio reindex --at`)。

## 1.7 AI Agent レスポンス契約

```json
{
  "query": "認証仕様",
  "requested_mode": "auto",
  "resolved_mode": "text",
  "fallback": true,
  "fallback_reason": "embedding_not_authorized",
  "error_code": "KIO-E-SEARCH-VEC-UNAUTHORIZED-001",
  "diversify": { "strategy": "mmr", "mmr_lambda": 0.7 },
  "paging": { "limit": 20, "next_cursor": "eyJ2IjoyLCJzY29wZXMiOls..." },
  "searched_scopes": [
    { "scope_id": "scope_01J8ZQ...", "scope_path": "/Users/foo/Research/.kio", "snapshot_at": "sha256:9f2c..." }
  ],
  "excluded_scopes": [],
  "index_status": {
    "enriched_ratio": 0.42,
    "pending_enrichment_tasks": 3120,
    "budget_paused": true
  },
  "aggregator": {
    "collection_generation": "sha256:..."
  },
  "results": [
    {
      "result_type": "chunk",
      "chunk_hash": "sha256:...",
      "evidence_pointer": {
        "schema_version": 1,
        "commit": "sha256:9f2c...",
        "tree": "sha256:3f9a...",
        "raw_hash": "sha256:...",
        "tool_profile_hash": "sha256:...",
        "chunk_hash": "sha256:...",
        "path_at_commit": "report.pdf",
        "heading_path": ["認証仕様", "API Token"],
        "byte_start": 1200,
        "byte_end": 1500,
        "scope_id": "scope_01J8ZQ..."
      },
      "evidence_uri": "kio://scope_01J8ZQ.../sha256:9f2c.../sha256:.../sha256:.../sha256:...",
      "related_images": [
        { "image_uri": "kio://scope_01J8ZQ.../object/image/sha256:a1b2...", "order": 0 }
      ],
      "score": 0.87,
      "scope_path": "/Users/foo/Research/.kio"
    }
  ]
}
```

`aggregator` は cursor が凍結する `collection_generation` を返す (§1.8)。
**2026-08-11: `applied` と `fallback_reason` を `aggregator` object から削除した** — 検索は
scope 数によらず必ず replica が採点するため、「適用されたか」も「なぜ適用されなかったか」も
存在しない。残るのは `collection_generation` だけである (cursor の凍結対象 — §1.8)。

**成功応答 (exit 0) の `error_code` は縮退原因の機械可読分類であり、失敗判定には使わない** — 失敗判定は exit code (非 0) が正 ([06-cli-spec.md §7](06-cli-spec.md)。上例は vector 未承認の text fallback で、`results` は有効な結果である)。`evidence_pointer` は [08-evidence-pointer-spec.md §2](08-evidence-pointer-spec.md) の schema を **そのまま** 埋め込む。root (`.kio`) の信頼は `evidence_pointer.scope_id` を正とし、`results[].scope_path` は解決を高速化する表示・ヒント用の絶対パスである (truth vs cache の不変条件。解決手順は [08-evidence-pointer-spec.md §3.1](08-evidence-pointer-spec.md))。

`evidence_uri` は Evidence Pointer の正規テキスト形 ([08-evidence-pointer-spec.md §2.3](08-evidence-pointer-spec.md)) であり、そのまま `kio open` / `kio view` / `kio evidence verify` の引数に渡せる。

`index_status` は AI 強化 (Markdownize / Embedding) が全対象に行き渡っていないときのみ必須 (`enriched_ratio < 1.0`)。人間向け表示では「AI 強化 42% (budget により一時停止中)」のような 1 行警告に翻訳する。

`snapshot_at` と `evidence_pointer.commit` の決定規則:

- `searched_scopes[].snapshot_at` = 当該 scope の検索対象 commit。デフォルト / `--all-history` / `--include-deleted` では検索時の HEAD commit、`--at` では指定 commit
- `evidence_pointer.commit`: デフォルト / `--at` では検索対象 commit。`--include-deleted` の live chunk は
  snapshot HEAD、削除済み分は final binding を選んだ newest first-parent commit。これにより
  `path_at_commit` は pointer commit の tree に必ず実在する。`--all-history` / `--since` は distinct
  `(chunk_hash,path)` ごとに、全 parent DAG 上の canonical introduction commit を使う。introduction は
 「その commit に binding が存在し、利用可能な全 parent に存在しない」commit。delete/re-add を含む
  複数 introduction のうち別 introduction の descendant でない ancestor-most 集合を作り、1 件ならそれ、
  複数の incomparable 候補なら full commit hash の bytewise 辞書順最小を使う
- `path_at_commit` = `evidence_pointer.commit` の tree における path

`--all-history` / `--since` は同じ chunk の同じ path が複数 commit に現れても 1 hit に畳む一方、rename
で生じた distinct path は別 hit として返す。各 historical alias result は、同じ raw_hash を持つ page-1
snapshot HEAD entry の distinct path を UTF-8 byte order で整列した `current_paths` として持つ
(空なら field を省略)。
raw identity から rename lineage は推測しない。`current_paths` がちょうど 1 件のときだけ compatibility
field `current_path` に同じ値を入れ、identical-byte twins では singular field を省略する。chunk 行自体は
path 非依存で 1 行のまま、path alias は snapshot HEAD から全 parent DAG の tree と snapshot HEAD tree
から導出する。

実装 pipeline は固定する: aggregate の text / vector / image lane を collection 全体で rank → RRF →
global MMR / `max_per_raw_hash` → resolver binding の alias 展開、の順に行う。scope ごとの rank、
cross-scope merge、または vector rank の後段再採点は行わない。pre-alias tie は immutable
`(scope_id,chunk_hash)` の UTF-8 byte order とする。その確定 semantic position ごとに historical/deleted aliases を展開し、parent
score/rank をコピーして、group 内を
`(scope_id,chunk_hash,path_at_commit,evidence_pointer.commit)` の UTF-8 byte order で整列してから paginate
する。`scope_path` は display hint なので順序に使わない。alias は MMR cosine competition や
`max_per_raw_hash` へ再投入せず、distinct alias は path/commit により comparator equality にならない。

### result 行が指すもの — pointer と payload の分離 (2026-07-26 確定)

Kio の主たる消費者は LLM Agent であり ([06-cli-spec.md §9](06-cli-spec.md) — MVP の導線は
`kio search --json` + `kio open`)、**適切な画像を Agent へ渡すことは Kio の役割に含まれる**。
一方で本契約が返すのは pointer であって本文ではない (上例のとおり chunk 本文すら含まない)。
両者を混同しないため、result 行のフィールドを役割で分ける。

| field | 役割 | 省略規約 |
|---|---|---|
| `result_type` | `"chunk"` \| `"image"` — この行が何を指すか | **常に必須** |
| `evidence_pointer` | **引用の不変固定**。time-travel と `kio evidence verify` が成立する | 常に必須 (従来どおり) |
| `payload_uri` | **Agent が `kio open` して実体を得るハンドル** | `result_type: "chunk"` では省略 (実体は chunk 自身であり `evidence_uri` が既にそれを指す) |
| `related_images[]` | この chunk 本文が参照している画像の列挙。`{image_uri, order}` の配列 | **空なら field ごと省略**。かつ `result_type: "chunk"` の行のみ (image 行では列挙対象が「参照元 chunk の図」になり、その先頭は自分自身の `payload_uri` である) |
| ~~`snippet`~~ | **廃止 (2026-08-11、§1.7.1)。**クリックで正規化 Markdown 全体を表示するため、リストに抜粋を載せる意味が無い | 返さない |
| `title` | **検索している時点の文書名** (§1.7.1 の時点規則)。従来は `path_at_commit` の字面 | 常に必須。意味は型に依らない (どちらの行も同じ chunk pointer を指す) |
| `changed_at` | **内容が最後に変わった commit の日時** (§1.7.1)。resolver が選んだ qualifying publication introduction の日時。リネームでは動かない | 常に必須 |

### 1.7.1 結果は文書単位 (2026-08-11 確定)

設計の全文と経緯は
[tasks/search-result-presentation-design.md](../tasks/search-result-presentation-design.md)。

**(1) 集約 — 1 文書 1 行。**キーは **`(scope_id, raw_hash)`**。

- **リネーム (内容不変)** は `raw_hash` が同じなので **1 行に畳む。**同じ内容を
  複数行見せる意味は無い
- **編集**は `raw_hash` が変わるので**自然に別行**。特別扱いではない
- **同一内容が複数 scope にある場合は畳まない。**バイト列が同一でも
  **階層が違えばアクセス権が違いうる**ため、1 行に畳むと到達できない経路を
  到達できるように見せるか、逆に隠す。**内容の同一性は権限の同一性を含意しない**

`--limit` / `--offset` / cursor の単位も**文書**になる (従来は chunk)。
§1.4 の `max_per_raw_hash` は、集約後は「同一内容が複数 scope にある場合」にのみ
効く上限として意味が変わる。

**(2) 文書名 — 検索している時点の名前を返す。**

| 検索 | `title` |
|---|---|
| 現行ツリー | 現在の名前 |
| `--at C` | **C 時点の名前** |
| `--all-history` | その内容が存在した**最後の commit 時点の名前** |

**`raw_hash` に path を混ぜない** ([03-data-model.md §5](03-data-model.md) の
`sha256(原本バイト列)` を変えない)。混ぜると (a) `(raw_hash, tool_profile_hash)` の
up-to-date 判定が外れてリネームだけで**再 OCR が走り**、(b) `chunk_hash` が
`raw_hash` を入力に持つため**発行済み Evidence Pointer が全て切れ**、
(c) content-addressed store の dedup 前提が崩れる。**path は commit の tree が
持っており失われていない** — 足りなかったのは「どの時点の視点で名前を出すか」の
規則だけだった。

**(3) 本文はリストに載せない。**`--limit` は最大 100 で、全文を載せると応答が
桁違いになる。加えて result 行が**検証可能な span** を指すことが
`kio evidence verify` を成立させている。**リストは span、実体は別途取得**の二段。
`snippet` の廃止も同じ理由による。

### 1.7.2 選択したときに表示するもの (2026-08-11 確定)

**リストはパスを返し、選択したらそのリンク先を表示する。**本文を返す API は作らない。

リンク先は **全文 view** `objects/normalized/ab/cd/<raw64>.<tool64>.g<gen>.md`
([03-data-model.md §2.1](03-data-model.md))。既に実体があり
(`kio_pipeline::markdownize::normalized_view_path`)、再生成可能な cache である。

- 表示は**正規化 Markdown 全体**
- 初期スクロール位置は**該当 chunk が画面中央**
- **ハイライトしない。**フラットに見せる (図の閲覧 UI として適切という判断)

**`kio view` はチャンク本文を返すのをやめ、この view のパスを返す。**
`kio open` は従来どおり**原本**のパスを返す。役割が名前どおりに分かれる:

| | 返すもの |
|---|---|
| `kio open` | **原本**のパス (PDF などをそのまま開く) |
| `kio view` | **全文 view** のパス + view-local span (下記) |

**view-local span を返すこと。**[03-data-model.md §2.1](03-data-model.md) の組み立て規則 5 の
とおり、chunk の `byte_start` / `byte_end` は **unit-local** であり、全文 view には
ヘッダコメントと unit 間の `"\n\n"` 結合が入る。**pointer の span をそのまま view の
オフセットとして使うと位置がずれる。**変換は unit の view 内開始位置を知る側 —
すなわち `kio view` — が行う。クライアントに組み立て規則を再実装させない。

`related_images[]` の抽出規則 (決定論。推論も追加索引も行わない):

- 対象は chunk 本文中の **Markdown 画像参照** `![alt](kio://<scope_id>/object/image/<hash>)` —
  [07-adapter-spec.md §5.2](07-adapter-spec.md) の画像参照置換が発行する形。
  リンク (`[text](uri)`) や地の文に現れた URI は対象にしない
- **`image_uri` は本文の字面をそのまま返す。** 正規化しない
  ([08-evidence-pointer-spec.md §2.3](08-evidence-pointer-spec.md) — object URI は opaque に扱い
  `scope_id` の大文字小文字を保存する)。`kio open` は URI が宣言する scope authority だけを解決し、
  別 scope に同一 hash の bytes があっても差し替えない
- **同一 URI が複数回出現する場合は最初の 1 件に畳む。** `order` は畳んだ後の
  出現順 (0 始まり) — 同じ画像を 2 回返しても Agent には情報が増えず `kio open` が
  重複するだけであるため
- **不完全な参照は落とす (fail-empty)。** chunk は normalized unit 本文の byte span であり
  ([03-data-model.md §8.1](03-data-model.md))、`[chunking].max_chars` の切断が参照の途中に
  落ちると URI が分断され得る。閉じ括弧を欠く断片・64 桁に満たない hash などは抽出しない —
  誤った hash を持つ URI を返すより安全側である
  - **分断された画像は `related_images[]` から消えるだけでなく、埋め込みも受けない。**
    どの画像を埋め込むかを決めるのが同じ抽出器であるため、その画像は
    (chunk 本文からは) 検索に一切現れなくなる。archive 側は無傷で、
    正規化 unit 本文と CAS object はどちらも完全なまま残る — 到達性判定は
    chunk ではなく unit を読むからである
  - **起こる条件は運ではない。**[04-pipeline.md §4.1](04-pipeline.md) の分割規則 5 は
    window 内の最後の空行で切り、空行が 1 つも無いときだけ文字位置で切る。
    したがって参照が分断されるのは **`max_chars` より長い「空行を含まない連続領域」の
    内側だけ**である (実測は `tasks/local-adapter-plan.md` の V7)

- **面積が下限に満たない画像は列挙から外す。** 同一 unit が記録した最大の図
  ([07-adapter-spec.md §5.2](07-adapter-spec.md) の `metadata["images"][].bbox`) に対する
  面積比が `[search] related_images_min_area_ratio` (既定 **0.25**) 未満のものは返さない。
  ページ上の画像の大半は、隣の本文が既に述べている内容の装飾であり、
  `related_images[]` の 1 件は Agent の `kio open` 1 往復を意味するため
  - **比の分母は chunk ではなく unit の最大図である。**装飾しか含まない chunk は
    空で返るのが正しく、その中で最も大きい装飾を昇格させてはならない
  - **bbox が記録されていない画像は落とさない。**測れないことと小さいことは違う
  - **`bbox` が読めない場合 (purge 済み・旧 gen・provider が箱を返さない) は絞らない。**
    知り得ないことは、Agent に渡す情報を減らす理由にならない
  - 既定値は実測 4 ページに基づく (装飾の上限 10.7% / 図の下限 83.6%)。
    **索引時には何も捨てない**ので、値を変えれば次の検索から再索引なしで効く
  - **設定の層は他の `[search]` キーと同じ (PC49/PC50)。**folder の値が効くのは
    単一かつ `--descendants` でない `--scope <path>` のときだけで、
    multi-scope 検索は user (device) 層のみを読む — 1 つの応答に 2 つの下限は持てない
  - この絞り込みは**この応答の見せ方だけ**に効く。到達性判定 (purge の orphan 判定・
    scope 射影) は絞る前の完全な列挙を使う — さもなければ装飾が孤児になる

**画像ヒットは `payload_uri` に画像オブジェクト URI、`evidence_pointer` に参照元 chunk を持つ。**

```json
{
  "result_type": "image",
  "chunk_hash": "sha256:...",
  "evidence_pointer": { "...": "参照元 chunk の pointer (08 §2 の schema をそのまま)" },
  "evidence_uri": "kio://scope_01J8ZQ.../sha256:9f2c.../sha256:.../sha256:.../sha256:...",
  "payload_uri": "kio://scope_01J8ZQ.../object/image/sha256:a1b2...",
  "score": 0.81,
  "scope_path": "/Users/foo/Research/.kio"
}
```

`evidence_pointer` を画像オブジェクト URI にはできない。
[08-evidence-pointer-spec.md §2.3](08-evidence-pointer-spec.md) のとおり
`kio://<scope_id>/object/image/<hash>` は **object 参照であって Evidence Pointer ではなく**、
commit も tree も `path_at_commit` も持たないため時点指定も検証も成立しないためである。
参照元 chunk を pointer に据えることで、**画像を渡しつつ引用の不変性・検証可能性・
time-travel を保てる**。

**参照元 chunk が複数ある場合は `chunk_hash` の UTF-8 byte order 最小のものを選ぶ。**
この tie-break は §1.3 (RRF) / §1.4 (MMR) / 本節 (alias 整列) で既に横断使用している idiom で
あり、`chunk_id` の値は chunk object の `chunk_hash` と同一文字列である
([04-pipeline.md §4.1](04-pipeline.md))。**SQLite の rowid 順は採らない** —
`index/sqlite.db` は `objects/` から再構築可能な cache であり
([04-pipeline.md §4.3](04-pipeline.md))、rowid は `kio repair rebuild-db` をまたいで安定しない。
Agent が保存し後から検証する**永続的な引用**の選択根拠に cache の再構築順を使うと、
rebuild 後に同じ検索が別 chunk を引用し得る。`chunk_hash` は content-addressed identity 由来で
rebuild に不変である。**逆引きの探索範囲は検索対象 commit に限る** (§1.6 の既定と同じ) —
`chunks` 行は purge 以外で削除されないため、限定しないと旧 gen の chunk が候補に残る。

**image 行は vector lane からのみ生じる** (2026-07-26 確定)。`--mode text` や
embedding 未承認の text fallback (§1.1) では image 行を返さない。画像が自前で持つ得点は
vector だけであり、参照元 chunk の text rank だけで順位を付けると**その chunk の重複行を
別名で返す**ことになるためである。text 経路でも図には到達できる — chunk 行の
`related_images[]` がそれを担う。

`related_images[]` は **参照の列挙であって存在保証ではない。** purge 済み画像の URI が
chunk 本文に残ることがある。検索時に存在確認 I/O は行わず、終端は `kio open` 側の既存 barrier
(`KIO-E-PURGE-NOT-FOUND-001`) が担う。

**検索レスポンスに画像を base64 で埋めてはならない。** 実体の受け渡しは常に URI 経由とし、
Agent は `kio open` でバイト列 (キャッシュパス) を得る。base64 は Agent の
コンテキストとコストを直接圧迫するうえ、本契約が pointer を返す設計と矛盾する。

## 1.8 複数 scope 横断検索 (multi-scope search)

> **2026-08-12 実装更新:** `kio search` の候補選択・採点・結果 materialize は
> `aggregator.sqlite` の単一路で行う。検索中に `.kio/index/sqlite.db` を開いて refresh / resolver を
> 再実行しない。完全な replica 射影は writer / repair の責務であり、通常 writer の replica 射影失敗は
> command を失敗にせず、既存 header があれば `Rebuilding` として検索を fail-closed にする（header が
> 無ければ projection 欠落として fail-closed、purge は射影完了まで command 自体を失敗にする）。履歴 selector
> と cursor replay は source **CAS** から exact binding relation を再解決し、それを replica 内の runtime
> eligibility filter として `candidate_depth` より前に適用する。これは source candidate SQL / materialize /
> 射影修復への fallback ではない。source CAS はこの selector / shallow 確認と、候補に現れた scope の
> purge journal / epoch / lifecycle barrier の権威確認にだけ用いる。

デフォルトの `kio search` は scope_registry に登録された全 indexed scope を対象とする ([06-cli-spec.md §3](06-cli-spec.md))。

**実行モデルは replication である (2026-07-25 変更)。** 全 scope の chunk (live + 過去 — 2026-08-11) を device-level の
read replica (`aggregator.sqlite` — [03-data-model.md §4](03-data-model.md)) へ複製し、**単一コーパスの上で
1 回採点する**。従前の scatter-gather (scope ごとに独立クエリ → per-scope 順位で RRF マージ) は
**2026-08-11 に廃止した** (下記「aggregator が答える条件」)。多 scope 検索の経路は replica 1 本である。
**scope 数によらず、検索は replica だけを引く** — 1 scope でも同じである (下記「検索は
`.kio/index/sqlite.db` を引かない」)。

変更理由は採点の正しさである。`.kio` ごとに独立した index は `.kio` ごとに独立した BM25 コーパスを
意味し、コーパス統計 (N / df / avgdl) が index ごとに異なるため **text 順位は scope 間で比較不能**である。
にもかかわらず RRF は text 項と vector 項を加算するので、per-scope 順位と global 順位が同一スケールで
足される。実測 (428 scope・中央値 6 chunk) では「6 チャンクのフォルダで 1 位」と「3,851 チャンク全体で
1 位」が同額になり、正解が横断 38 位へ沈んだ。**コーパスを 1 つにすればこの問題は定義上消える** —
正規化関数もチューニング定数も要らない。分散 IR の古典解 (Elasticsearch の `dfs_query_then_fetch`) と
同じ問題・同じ答えである。

### 対象 scope の列挙

1. scope_registry から `participates_in_global_search = true` の scope を列挙する
2. `--scope <path>` 単独指定は canonical root_path の**完全一致** (当該 scope のみ — [06-cli-spec.md §3](06-cli-spec.md) の「カレントフォルダのみ」)。`--descendants` 併用時は self + 「`root_path + '/'` を前置に持つ scope」を対象とする (**path-component 境界で判定** — 単純な文字列前方一致は `/work/a` が `/work/ab` に一致するため用いない)。**canonical root_path の算出規則**: CLI 入力を (1) 絶対化 (cwd 基準)、(2) `.` / `..` の lexical 解決、(3) 末尾 separator 除去、(4) symlink 解決 (realpath) の順で正規化する。比較は **byte 単位** (case-folding しない — case-insensitive filesystem では観測された実 path 表記を正とする)。scope_registry の `root_path` も同一規則で保存する ([10-operations.md §3](10-operations.md))
3. 到達不能 / stale な scope (外部ドライブ切断等) は skip し、`excluded_scopes` に理由付きで記録する (検索全体はエラーにしない)

### replica の内容 — 解決済み binding を伴う単一コーパス

aggregator は **生テーブルを複製して eligibility を再実装しない**。writer / repair の射影時に scope 側の既存 resolver が
`current` / `all_history` / `include_deleted` / `at` の各検索条件について解決した答えを受け取り、
`agg_bindings` として射影する ([03-data-model.md §4](03-data-model.md) 不変条件 7)。この relation は
`scope_id`、selector 種別、snapshot commit、chunk identity、`path_at_commit`、`pointer_commit`、
現在 path、live フラグを持つ。config の切替、DAG の分岐・再導入、同一 chunk の複数 alias を
**scope resolver の出力どおり**表せるため、scalar metadata だけで履歴の可否を推定しない。

`agg_chunks` は全 committed chunk を一度だけ保持する単一の検索表である。候補時の可否は、query ごとの snapshot と
`agg_bindings` から作る eligible chunk 集合を `WHERE EXISTS` で参照して決める。alias binding を join で
候補行へ増殖させないため、同じ chunk の複数の履歴 path が rank を重複させることはない。

fresh current 検索は durable な `agg_bindings` を使う。対して履歴 selector と**全 cursor replay**は、CAS
だけで再解決した exact binding relation を request-scoped runtime filter として `agg_bindings` に交差させる。
この交差は replica connection 内で候補深さより前に評価されるため、後から shallow になった ancestor の
alias や selector と合わない古い投影を返さない。これは source SQLite を開く fallback でもなく、CAS から候補の本文・
vector・Pointer metadata を materialize する経路でもない。

| 表 | 内容 |
| --- | --- |
| `agg_scopes` | `scope_id` PK / current snapshot・config / `index_generation` / projection の `max_rowid`・`max_association_rowid` / embedding profile / `index_status` / `refreshed_at` |
| `agg_chunks` | 全 committed chunk を一度だけ保持する検索表。`raw_hash` / profile / generation / text / heading / `section_id` / byte span / unit・作成 metadata を持つ |
| `agg_fts` | `agg_chunks` を external content とする **全 scope 単一の FTS5** — `bm25()` は常に collection 全体で計算される |
| `agg_embeddings` | 全 chunk 分の vector: `chunk_rowid` / `scope_id` / `vector` / `dimensions` |
| `agg_image_embeddings` / `agg_image_refs` | 画像 vector と、その画像を引用する eligible chunk・URI の relation。画像本文用の第 2 FTS は作らない |
| `agg_bindings` | scope resolver が解決した selector / snapshot ごとの chunk binding。検索対象を選ぶ relation であり、別コーパスではない |
| `agg_projection_markers` | selector / snapshot の完了 marker。binding 0 行の有効な空答えを cache miss と区別し、config と shallow skip も固定する |

**live と履歴を分表にしてはならない。**`--include-deleted` は生存・削除済みを同じ順位で返し、
`--all-history` / `--since` / `--at` も同じ `agg_fts` と vector collection を使う。binding による `WHERE`
絞り込みは返却候補だけに効き、FTS の `N` / df / avgdl は collection 全体のままである。こうして
候補深さの上限は eligibility を満たす行に対して適用され、live/history を別 collection の順位として
混ぜることはない。

候補 materialize に必要な Evidence Pointer metadata は `agg_chunks` と `agg_bindings` にあり、
画像候補の引用先も `agg_image_refs` から選ぶ。したがって `Aggregator::search_candidates` が始まった後に
候補や Pointer を組み立てるため `.kio/index/sqlite.db` を再び開くことはない。

#### 書き込み順序 — 各 scope の索引が先、aggregator が後 (2026-08-11 確定)

**変更は必ず当該 scope の `.kio/index/sqlite.db` へ先に反映し、その後で aggregator に伝える。
逆順を実装してはならない。**

順序が効くのは、途中で失敗したときの壊れ方が**非対称**だからである。

| 順序 | 中断すると |
|---|---|
| **scope → aggregator (規定)** | writer は完全な replica 射影を試行する。通常 writer の replica 射影が途中失敗しても command は失敗にせず、既存 header があれば `Rebuilding` として検索を fail-closed にする（header 無しも projection 欠落として fail-closed）。purge は replica 消去を確認できなければ command 自体を失敗にする。いずれも検索時に source を読んで補わない。**検索に出た結果は必ず開ける** |
| aggregator → scope (**禁止**) | **replica が、scope に無い chunk を持つ窓ができる。**検索は replica しか読まないのでその行が結果に出るが、Evidence Pointer は live `.kio` で解決される (08 §3.1) ので**開けない結果**になる |

**検索が replica しか読まないことが、この順序を必須にしている。**replica が先行できると、
先行分は「検索には出るが実体が無い」という形で**そのまま利用者に見える**。逆順ならば
先行するのは scope 側で、その分は検索に出ないだけで済む — **遅れて見えるのは安全、
先に見えるのは危険**という非対称である。

03 §4 不変条件 1 (`scope_registry` / aggregator のみで `.kio` の状態を変える実装は禁止) の
**書き込み順序版**であり、承認の一括操作 (§1.9 「aggregator を先に書いて `.kio` を後で
追随させる実装は禁止」) は既に同じ規則を持つ。索引にも同じ規則を当てる。

#### 更新方式 — 書き手が replica に伝える (write-through、2026-07-25 確定)

**正本を書いた処理が、同じ処理の中で完全な scope 射影を replica に書く。** 読み手は
`index_generation` の差を見て source を開いたり、欠けた binding を補ったりしない。**上記の順序
(scope が先、replica が後) を守り、射影不能・不完全なら既存 header は `Rebuilding`、header 無しは
projection 欠落として検索を止める。**

読取り時の lazy refresh は撤去した。replica の世代・必須 binding・本文 metadata が要求を満たさない
場合、`kio search` は source SQLite へ fallback せず当該 scope を fail-closed とする。通常 writer は
source 側の成功を取り消さないが、既存 replica header を `Rebuilding` にする。selector の CAS preflight は
歴史 selector / cursor の runtime eligibility filter を得るための正本確認であり、source から候補や binding を
materialize / 修復する経路ではない。単一 scope の復旧は `kio index` / `kio reindex` /
`kio repair rebuild-db`、device replica 全体の復旧は `kio repair replica` (`-r`)、source の検証・
SQLite 再構築も含む全体復旧は `kio repair all` (`-a`) が行う。**purge だけは replica の完全消去も command 成功の
必須条件**である (§3.5)。

| 経路 | live index への書き込み | replica への反映 |
|---|---|---|
| 索引の再構築 (`index` / `reindex` / `repair rebuild-db`) | temp DB + rename | 完全射影を試行し、`agg_chunks` / binding / vector / image relation を置換。通常の射影失敗は command を失敗にせず、既存 header があれば `Rebuilding` にする |
| lifecycle（purge 以外） | in-place | 完全射影を試行。通常の射影失敗は command を失敗にせず、既存 header があれば `Rebuilding` にする |
| purge | in-place | 完全射影。replica 本文の消去を確認できなければ command を失敗にする |
| Batch / 同期 enrichment / link | in-place | 完全射影。vector だけの差分適用で検索時の補完を期待しない |
| `reindex --at <commit>` の投影 | in-place (`chunks` — **本文**) | 完全射影。要求された履歴 binding と、空答えを含む selected target の completed marker を publish |

write-through は**各 writer の完全性条件**である。`persist_group_vector`、`rebuild_sqlite_index` の rename、
purge、履歴 reindex を含め、source index を変える経路は最終的に同じ完全射影を試行しなければならない。
通常 writer の replica 射影失敗は command を失敗にせず、既存 header があれば `Rebuilding` marker として残す。
header が無い場合も projection 欠落のまま reader を fail-closed とする。purge だけは完全射影を完了できなければ command を失敗にする。
このため検索時に変化検知や欠損補完を積み残す余地が無い。

**すべての writer は全置換の完全射影を試行する。** `refresh_scope_with_projection` は対象 scope の
`agg_chunks`、FTS、chunk / image vector、画像引用 relation、selector / snapshot binding を一つの
射影単位として delete-then-insert で置換する。本文を変えない vector 更新も例外にしない。差分だけを
当てて「不足分を次の検索が補う」設計は持たない。

完全射影が必要なのは、scope が持たなくなった chunk を確実に落とし、collection の document frequency を
正しく保つためでもある。部分 projection や generation だけの更新を許すと、本文・binding・vector の
いずれかが欠けたまま検索に見える。したがって writer は projection の完了を確認してから generation を
publish する。通常 writer の projection が失敗した場合は source 側の成功を取り消さず、既存 replica
header を `Rebuilding` にして reader を fail-closed とする。既に不完全な replica を見つけた reader は
source SQLite を開かず、次の writer / repair に完全射影を要求する。**purge は例外で、replica 本文の
消去を確認できなければ成功を返さない** (§3.5)。

**スタンプは完全射影の後に書く。** generation スタンプは projection の commit marker である。
DB を跨ぐ atomic commit が無い以上、途中失敗を「後で読取りが直す」と扱わず、`Rebuilding` marker で
検索を止めることで整合性を守る。

#### `index_generation` が回転する経路

回転は cursor を無効化するために必要である。順位が変わりうる変更は replay 中の cursor を
退役させなければならない。**索引を in-place に変える全経路が回転し、その writer が完全射影を
publish する。**スタンプは reader-side repair の信号ではない。

1. 索引の再構築 — 新しい temp DB に新 ULID を入れて rename する
2. lifecycle / purge (`rotate_index_generation`)
3. **Batch レーンの埋め込み回収** (2026-07-25 追加) — ベクタが増えれば順位は変わる
4. **同期レーンの埋め込みと内容アドレス再利用の link** (2026-07-26 追加) — `kio index` 経路では
   直前の再構築が既に回転させているが、**`batch resume` で batch レーンが使えず同期ループへ落ちた場合は
   他に何も回転しない**
5. **`reindex --at <commit>` の投影** (2026-07-26 追加) — 本文 corpus を再公開する以上、cursor replay の
   順位が変わりうるのは当然である

**回転させないもの**: 検索 collection / binding / vector が変わらない更新。全 member が secrets hold
の group は `content_vectors` 行を持っても検索用 vector を公開しない
([03-data-model.md §4](03-data-model.md) 不変条件 8) ため、cursor の順位を変えない。

### 実行とマージ

1. **projection 完全性と selector の事前確認**: `kio search` は `agg_scopes` と `agg_bindings` を
    確認し、要求 selector / snapshot に必要な完全 projection がある scope だけを参加させる。欠落・
    不完全・`Rebuilding` の projection は source SQLite を開いて refresh せず fail-closed とする。`--at` 等の
    canonical target / shallow 判定には source CAS を正本として用いてよい。履歴 selector と cursor replay は
    同じ CAS planner から exact binding relation を得て request-scoped runtime filter にする。この filter は
    `agg_bindings` と交差させるだけであり、chunk / binding / metadata を source SQLite から読み出したり
    replica を修復したりする経路ではない。
2. **単一コーパスから候補を選択して採点**: `Aggregator::search_candidates` は query snapshot と
    `agg_bindings`（履歴 / cursor では上記 runtime filter との交差）から eligible chunk を作り、`agg_fts` の
    1 回の `bm25()`、`agg_embeddings` の cosine、および短語の bounded `instr` を collection 全体に対して
    実行する。この eligibility を満たした行に対して各 lane の candidate_depth を適用し、得た global rank を
    RRF で直接加算する。
3. **purge barrier（候補 scope のみ）**: replica が候補を選んだ**後**、候補に含まれる distinct scope だけ
   `ReadBarrierCheckpoint::open` を行う。active purge journal を検出した scope は、その全候補を除外して
   `excluded_scopes` に記録する。通過した checkpoint は応答境界の直前に `recheck()` し、実行中に
   journal / purge epoch / lifecycle epoch が変わった scope は、その body を返却せず同様に除外する。これは per-scope 候補生成に
   埋め込まれた旧 barrier ではなく、replica 候補経路の一部である。live `.kio` の control record を
   読む安全確認であって、source SQLite / CAS から候補や Evidence metadata を取り直す fallback ではない。

   **replica に安全性判定を委ねてはならない** ([03-data-model.md §4](03-data-model.md) 不変条件 6)。
   ただし毎回全 scope を開くのではなく、候補を出した scope だけ検証するため、検証コストは候補ページの
   distinct scope 数に比例する。回帰テスト `ct3_multi_021_replica_candidates_exclude_an_active_purge_scope`
   は、この barrier を外すと active journal の scope が結果へ戻ることを検出する。

#### 候補選択の所在 — replica（2026-08-12 実装済み）

**多 scope 検索の候補選択・採点・materialize はすべて replica が行う。** `kio search` は
scope 数や時間選択子で候補生成経路を分岐させず、source SQLite を読んで lazy refresh しない。
`Aggregator::search_candidates` は `agg_fts`、`agg_embeddings`、`agg_image_embeddings`、
`agg_image_refs`、`agg_bindings`、および歴史 selector / cursor に渡された request-scoped CAS filter だけから
候補を作る。filter は aggregator connection 内の一時 relation であり、source index を読む第 2 の候補経路ではない。

返却する candidate には text / vector の global rank、Evidence Pointer の全 metadata、解決済みの履歴
binding が含まれる。よって候補 materialize で scope index を引き直さず、`--all-history`、`--since`、
`--include-deleted`、`--at` でも CAS が許した alias だけを返す。shallow ancestor を skip した履歴は
`shallow_skipped` を伴う partial であり、読める tree の healthy binding は返したまま、古い durable binding
から alias を補完しない。pure-short query は
aggregate 上の bounded `instr` レーンを用い、FTS5 の trigram 下限によって別経路へ落ちることはない。

この直接経路と candidate-scope purge barrier は同じ実装単位である。候補を replica から返す変更だけを
先に行うと barrier が消えるため、active purge journal を除外する回帰テストを常に維持する。
4. **統合と materialize**: text / vector / image の各 lane が単一 collection で返した global rank を
   RRF で直接加算する。pre-alias の同点は immutable `(scope_id, chunk_hash)` で安定化し、resolver binding を
   展開して Evidence Pointer を組み立てる。per-scope rank の比較、BM25 raw score の正規化、または後段の
   global 再採点は行わない。
5. diversify (MMR / group_by_raw_hash, §1.4) は統合後の候補列に対して適用する。**multi-scope 検索の
   `[search]` 実効値 (**default_mode** / rrf / diversify / candidate_depth / fail_behavior) は
   user config (device 層) を用いる** — folder 値は `--scope` 単一指定時のみ適用する (scope 間で
   異なる folder 値の統合は定義しない。cursor が bind する実効値 (§1.5) もこの解決に従う —
   **ただし fail_behavior は挙動方針であり確定順序に影響しないため bind / query_hash preimage の
   対象外**)
6. vector / hybrid の横断条件は [03-data-model.md §7](03-data-model.md) に従う。embedding profile が全 scope で一致しない場合、横断部分は text (BM25 rank) のみで統合し、`fallback_reason` に記録する (**`--mode vector` 明示時は fallback しない** — profile 不一致の scope を KIO-E-SEARCH-VEC-INCOMPAT-001 の excluded_scopes として除外し、全 scope 除外なら error — §1.2 の「失敗時は error」と同じ)。各 selected scope は replica/source-index candidate を読む前に `kio_format_version == KIO_FORMAT_VERSION` を検証する。missing、非 string、malformed、older/newer、未知を含む任意の不一致は `KIO-E-STORE-VERSION-001` / exit 8 で**command 全体を直ちに停止**し、当該 scope を `excluded_scopes` や `fallback_reason` に入れて healthy scope の partial result を返さない。source SQLite への書込みは行わない。**全 scope の除外理由が同一 code の場合、command は当該 code とその単独実行時の exit を返す (一般規則)** — REBUILDING → exit 3・INCOMPAT → exit 8・journal (`KIO-E-PURGE-JOURNAL-ACTIVE-001` — §3.5) → exit 3・DUP → exit 3 (ユーザーの dedupe 後に回復可能 — [08-evidence-pointer-spec.md §4.3](08-evidence-pointer-spec.md) の registry_duplicate = 3 と同一分類)。理由が混在して全 scope 除外となった場合は通常の SCOPE-ALL-FAILED とし、**exit は除外理由の retryability で分割する — 単独時 exit 3 の code (REBUILDING・journal・DUP・timeout 等の retryable 系) を 1 つでも含めば exit 3、全て permanent 系なら exit 4** (横断規約の「4 = 再試行で進展しない」([06-cli-spec.md §7](06-cli-spec.md)) と整合 — retryable 理由の scope は再試行で回復し得る)。個別理由は excluded_scopes[].reason で判別する。embedding 承認の consent gate (§1.1) は**送信 gate であり per-scope の除外条件ではない** — 承認ゼロなら検索全体が text fallback (excluded_scopes には計上しない)。§1.1 の送信 gate を満たして送信された query vector は profile 互換な全参加 scope の vector 検索に用いる (未承認 scope も含む — 送信は 1 回であり scope 別の再送信は発生しない)

### replica の候補経路と cursor

aggregator を使うかどうかを決める decline / fallback 分岐は存在しない。single scope、複数 scope、
および全ての時間選択子は、同じ aggregate candidate query を使う。集約後に per-scope rank を再採点する
処理も無く、各 lane が返す collection 内の rank をそのまま RRF に渡す。

**pure-short query**（全 unit が FTS5 trigram 下限未満）も同じ経路である。query の同値形ごとに
`agg_chunks.text` の bounded `instr` 述語を作り、eligible binding を満たす行から決定的な順序で
`candidate_depth` 件を選ぶ。日本語の 2 文字語を含め、短語だから source 側の検索へ切り替わることはない。

**cursor は `collection_generation` を常に凍結する。**これは replica が保持する全 scope と各
`index_generation`、投影された `max_rowid` / `max_association_rowid` の hash である。global BM25 は collection 全体の df / `N` / `avgdl` を読むため、
検索対象外の scope を index しただけでも順位は変わり得る。generation が一致しなければ
`KIO-E-SEARCH-CURSOR-001` / `reason = collection_generation_mismatch` とし、cursor 無しの再実行を案内する。
全ページが同じ replica 経路を通るので、`collection_generation` を持たない search cursor は存在しない。

**cursor replay は collection を再定義しない。** `--scope`/`--descendants` が replica を prune しないのと
同じ理由で、replay の scope 集合は page 1 が凍結した古い一覧であり、それを権威として page 1 以降に登録
された scope を追い出してはならない (追い出すと `collection_generation` が page 1 の値へ復元され、
検出すべき collection 変化そのものを隠す)。

ただし cursor replay も CAS-only の selector preflight を省略しない。page 1 が固定した snapshot から
exact binding relation を再解決し、runtime filter を replica の eligibility に `candidate_depth` 前で交差させる。
replay 後に shallow になった selected snapshot は hard-fail、history / include-deleted の shallow ancestor は
`shallow_skipped` を伴う partial とする。いずれも source SQLite を開いて page 1 の durable binding を補完しない。

**絞り込み検索の採点は「行」を絞り「統計」は絞らない。** `--scope` / `--descendants` は
`Aggregator::search_candidates` が作る eligible chunk 集合を絞るため、aggregate candidate query は
参加 scope の行だけを返す。candidate_depth はこの eligibility の後に適用し、source 側の per-scope SQL を
実行しない。一方 BM25 の df / `N` / `avgdl` は `agg_fts` 全体の統計であり、部分集合ごとに
統計を取り直さない。そうすると folder ごとに別 collection を作り直し、replica が除いた per-corpus IDF の
不整合を復活させるためである。

### 検索は `.kio/index/sqlite.db` を引かない (2026-08-12 確定)

**経路は 1 本だけである。scope 数によらず、検索はアプリ配下の統一 SQLite
(`aggregator.sqlite`) を引く。**

| 用途 | 読む先 |
|---|---|
| **検索の候補選択・採点・materialize (`kio search`)** | **`aggregator.sqlite` のみ** |
| selector / shallow の CAS preflight（履歴 / cursor の runtime eligibility filter を含む）、候補 scope の purge barrier | 各 scope の source CAS / control record（filter は replica の candidate_depth 前に交差。候補 SQL・射影修復には使わない） |
| writer / repair による replica 射影 | 各 scope の `.kio/index/sqlite.db` |
| scope 内の索引構築・`kio repair` 等の保守 | 同上 |

**`.kio/index/sqlite.db` は検索面ではない。****同じ問いに答える派生索引を 2 つ持てば、
答えが経路によって変わる。**scope が 1 つでも 2 つでも、検索が読む索引は 1 つとする。

#### では per-scope の索引は何のためにあるか — replication の複製元である

検索が読まないからといって不要ではない。**aggregator から見れば、per-scope の
`.kio/index/sqlite.db` が複製元 (正本) である。**writer / repair の完全射影だけがこれを読む。**

`kio search` はこの複製元を開かない。header が `Rebuilding` または required binding 不足なら
fail-closed とし、次の writer / repair に完全射影を要求する。

**そしてこれが `.kio` の中にあることが、フォルダの可搬性を成立させている (2026-08-11 記録)。**
フォルダごと別の場所・別の device へ移すと索引も一緒に動くので、移動先の aggregator は
**`chunks.jsonl` と object store からの再導出ではなく、射影だけ**で当該 scope を取り込める。
`.kio` が自己完結していることの実利がここに出る — [01-positioning.md §7](01-positioning.md) の
「データ・所有権・権限の正本は各フォルダ直下の `.kio` に閉じる」を、検索索引の側から支えている。

**区分は `cache` のままである** ([03-data-model.md §4.1](03-data-model.md))。
`kio repair rebuild-db` が `chunks.jsonl` + object store から再構築できるためで、
**「aggregator にとっての複製元」と「`.kio` の中では再構築可能な派生物」は両立する。**
移動時に索引が失われても復旧不能にはならず、移動先で rebuild が要るだけである
(射影より遅いが、失敗ではない)。

1 scope を例外にしない理由は 3 つある:

1. **経路が 1 本なら、経路差による挙動の違いが原理的に発生しない。**2 本あれば、
   短語の扱い・tie-break・`candidate_depth` の効き方が経路ごとに分岐しうる
2. **権限と purge barrier の確認箇所が 1 つで済む。**2 本あれば両方に置く必要があり、
   片方に入れ忘れても気付かない。replica 経路の回帰テストが active purge journal の scope を明示的に除外する
3. **collection が 1 つなら rank の意味も 1 つである。**scope 数を理由に採点主体を変える必要がない

検索応答の `aggregator` object は **`collection_generation` だけ**を持つ — cursor が凍結する
対象であり、ページ間で collection が動いたことの検出に要る (§1.5)。replica の使用有無や
replica 固有の fallback 理由を表すフィールドは無い。§1.1 の text fallback が使うトップレベルの
`fallback_reason` は、この経路選択とは別の契約である。

**「どの段が採点したか」を機械可読にする必要も消えた** — 段が 1 つしかないためである。
これは撤回前、順位品質が落ちた検索 (委譲側へ落ちた検索) を検出するために要る、という理由だった。

### 撤回した理由 — 不変条件 2 は「無くても動く」を要求していない

撤回前は「scatter-gather 経路は削除せず reference 実装かつ fallback として存置する —
[03-data-model.md §4](03-data-model.md) 不変条件 2 が『aggregator 喪失は再構築可能』を要求する以上、
aggregator を失っても検索が成立しなければならない」と書いていた。**この推論は成り立たない。**

不変条件 2 の文言は「**再構築可能** (各 `.kio` を rescan)」であって「欠損中も動作」ではない。
再構築の経路は writer / repair にあり、aggregator を失った場合の正しい挙動は**検索が fail-closed し、
次の `kio index` / `kio reindex` / `kio repair` が全射影を完了するまで待つ**ことである。「別実装の検索」も
読取り時の lazy refresh も持たない。**不変条件 2 は writer-side の再構築で満たされる。**

第 2 の実装を持つ代償は、この節が自ら記録していたとおりである — 委譲経路では per-scope 順位が
使われるため、**そこへ落ちた検索は §1.8 が消したはずの欠陥をそのまま踏む**。
そして pure-short query という**最も普通のクエリ形が常にそこへ落ちていた** (上記)。
「意図した縮退」と書いていたものは、実際には既定の挙動だった。

既知の限界: RRF は**片方のレーンでしか到達できない文書に 1 項しか与えない**。語彙の重なりが無い文書
(raster PDF の OCR 結果に対する言い換えクエリ等) は text 項を構造的に得られず、vector 単独より
不利になりうる。これは replication で解決しない RRF 自体の性質であり、対処 (片レーン専用文書への補正、
enrichment 済み device での既定モード、Cross-Encoder 再ランク) は別途とする。

### 設定

```toml
[search.multi_scope]
parallelism = 4                 # writer / repair が同時に完全射影する scope 数の上限
                                # kio search は refresh を実行しない
per_scope_timeout_seconds = 2   # 超過 scope は excluded_scopes (reason=timeout)
```

### 部分失敗と exit code

| 状況 | 挙動 | exit code |
| --- | --- | --- |
| 全 scope 成功 | 通常結果 | 0 |
| 一部 scope 失敗 / stale / timeout | 結果を返し `excluded_scopes` に記録 | 3 |
| 全 scope 失敗 (除外理由が同一 code なら §1.8 の昇格規則で当該 code の単独時 exit。混在時は retryable 理由を含めば 3・全て permanent なら 4 — §1.8) | エラー (混在時 = `KIO-E-SEARCH-SCOPE-ALL-FAILED-001`・同一 code 昇格時は当該 code) | 3 / 4 (同一 code 昇格時は当該 code の単独時 exit — 8 等) |

### レスポンス契約の拡張

単一値の `snapshot_at` は採用せず、次の 2 フィールドを返す (§1.7 の例):

```json
{
  "searched_scopes": [
    { "scope_id": "scope_01J8ZQ...", "scope_path": "/Users/foo/Research/.kio", "snapshot_at": "sha256:9f2c..." }
  ],
  "excluded_scopes": [
    { "scope_id": "scope_01K3AB...", "scope_path": "/Volumes/ext/Research/.kio", "reason": "stale" }
  ]
}
```

`snapshot_at` は scope ごとの検索時点 snapshot (commit_hash, [03-data-model.md §8.1](03-data-model.md))。単一 scope 検索 (`--scope .`) でも同形式 (要素 1 個の配列) を返す。これは [06-cli-spec.md §9](06-cli-spec.md) の JSON output 契約 (searched_scopes / excluded_scopes / fallback_reason) と同一である。

### cursor の multi-scope binding

§1.5 の cursor は collection 全体の順序を凍結しつつ、参加 scope ごとの snapshot / config / consumed 状態を保持する:

```json
{
  "v": 2,
  "scope_mode": "all",
  "query_hash": "sha256:...",
  "query_vector_digest": "sha256:...",
  "time_travel": { "all_history": true, "since": "604800s" },
  "since_cutoff": "2026-07-13T00:00:00Z",
  "excluded_scopes": [],
  "scopes": [
    { "scope_id": "...", "snapshot_commit": "sha256:9f2c...", "max_rowid": 18234,
      "max_association_rowid": 20117, "index_generation": "01J...",
      "chunking_config_hash": "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
      "consumed": 40 }
  ]
}
```

`time_travel` は query hash に入る canonical selector object (default のみ field 省略)、`since_cutoff` は
`--since` のときだけ存在する。cursor はこの JSON の JCS と認証 tag を含む signed
base64url opaque token として返す。Step 4 cursor schema は `v=2`; 必須 config/association/selector
binding を持たない obsolete `v=1` は `KIO-E-SEARCH-CURSOR-001` で拒否する (cursor は durable artifact ではなく、これは旧 cursor を受理する後方互換 branch ではない)。

- `scope_mode` は検索対象 scope の指定方法 (all / `--scope` / `--descendants`)、`query_hash` は次の正準構成 (per-scope の対象 chunking config binding を含む — §1.5 の対象 config と同一): `"sha256:" + base16(sha256(JCS({ query: <NFC 正規化後のクエリ文字列>, mode: <解決後の実効 mode (text|vector|hybrid)>, chunking_configs: <{scope_id,chunking_config_hash} の scope_id UTF-8 byte order 配列>, scope_mode, scopes: <page 1 または直前 replay で実際に参加する active scope_id の昇順配列>, rrf: <[search.rrf] の実効値 (k / candidate_depth / w_text / w_vector — 変更は確定順序を変えるため cursor 誤用検出の対象)>, diversify: <[search.diversify] の実効値>, query_vector_digest: <実効 mode が vector|hybrid のときのみ — page 1 の device-local query vector の canonical float32 little-endian bytes に対する sha256。text mode ではキー省略>, time_travel: <--at/--all-history/--include-deleted/--since の実効値 (未指定キーは省略)> })))`。`limit` / `--offset` / `--cursor` / `--json` は**含めない** (ページング操作で hash が変わってはならない)。いずれも token 全体に 1 つで、別クエリ・別条件・いずれかの scope の別 chunking config での cursor 誤用検出に使う (不一致は `KIO-E-SEARCH-CURSOR-001` で拒否、§1.5)
- page 1 の `scopes` / `chunking_configs` は成功して実際に ranking へ参加した scope だけを含む。
  page-1 `excluded_scopes` は bounded `{scope_id,reason}` として signed token に保持するが active scope や
  query hash の config mapping には入れず、その cursor stream へ後から再参加させない。registry に後から
  現れた scope も入れない
- `snapshot_commit` は当該 scope の検索時点 snapshot (commit_hash)、`max_rowid` / `max_association_rowid`
  は snapshot 時点で index に取り込まれていた chunk / association の上限、`index_generation` は
  page 1 時点の当該 scope の世代 ULID (§1.5 — 不一致は cursor 拒否)、`chunking_config_hash` は
  page 1 の当該 scope の**対象 config** (デフォルト = **当該 scope の HEAD tree の値** (移行期間の
  扱いは [04-pipeline.md §4.6](04-pipeline.md))、時点指定 = 対象 tree の値 —
  §1.5 と同一)、`consumed` は当該 scope から既に返した件数。page 2 で**対象 config** の mapping
  (保存値と再計算値の比較 — current ではなく対象時点の値) が 1 件でも違えば query hash mismatch として
  cursor を拒否する。署名検証後も
  いずれかの field 欠落・型違い・範囲外は cursor error
- cursor 付き呼び出しで selector flag を省略した場合は signed `time_travel` を継承する。1 つでも selector
  flag を再指定した場合は canonicalize 後に token object と完全一致しなければ
  `KIO-E-SEARCH-CURSOR-001`。これにより既存の `search QUERY --cursor TOKEN` は履歴 mode と canonical
  `--since` duration を失わず、同じ selector を明示してもよい
- replay は token の active `scopes` だけを解決する。active scope が unreachable/corrupt/shallow に
  なった場合、global merge/MMR stream を安全に縮退再計算できないため、部分結果や next cursor を返さず
  cause-specific に hard-fail する (unreachable は `KIO-E-SEARCH-CURSOR-001` reason
  `active_scope_unavailable`、shallow は `KIO-E-COMMIT-SHALLOW-001`、store damage は
  `KIO-E-STORE-CORRUPT-001`)。cursor なしの fresh search を案内する。scope move は同じ `scope_id` として
  継続し、config drift も cursor error とする
- 次ページは replica を cursor が記録した replica 世代に固定して再クエリし、global MMR → alias 展開まで 再計算した**最終 stream 上で** scope ごとの consumed 件を skip して継続する (per-scope の事前 skip は global 選択を変えるため行わない — §1.5 の consumed 定義が正本)。マージは決定的 (RRF スコア降順 + 辞書順 tie-break) なのでページを跨いで再現可能。(2026-08-11: scope 数による経路の分岐は無い — 検索は常に replica を引く)
- cursor 中の `snapshot_commit` が shallow 化済み (tree 破棄) の場合、cursor の再計算は `KIO-E-COMMIT-SHALLOW-001` で失敗する (§2.2)。この場合は cursor なしの再検索を案内する

### 性能目標の前提

M3-1 の p95 < 5 秒 ([09-mvp-scope.md §4.1](09-mvp-scope.md)) は **20 scopes / 合計 10 万 chunk** を前提とする。

replication の**候補選択と採点**は scope 数に依存しない — `aggregator.sqlite` への 1 回の query である。
`kio search` が scope の source を読む仕事は、selector の CAS 正当性 / shallow 事前確認と runtime eligibility
filter の解決、および候補に現れた scope の purge barrier に限られる。`.kio/index/sqlite.db` を開く source projection は writer / repair の
仕事であり、検索の性能経路には含めない。

ここに以前記録していた 428 scope の差分ゼロ検索値は、per-scope 候補生成が残っていた移行前の測定であり、
現在の経路の性能根拠には使わない。移行後の評価コーパスによる測定値は [09-mvp-scope.md](09-mvp-scope.md) に記録する。

`--scope` / `--descendants` と `participates_in_global_search = false` は対象範囲を定義する。絞り込みは返却行と
candidate-scope barrier の対象を狭めるが、BM25 統計は一貫して device-level collection のままである。

# 2. Commit / Snapshot

## 2.1 commit_type enum

commit は CAS JSON object であり SQLite に commit 表は存在しないため
([04-pipeline.md §4.4](04-pipeline.md))、**enum の強制点は commit object の schema
検証 (publication 時の loader)** である。値域 (JSON Schema enum 相当):

```text
commit_type ∈ { 'manual', 'auto', 'repaired', 'purged' }
```

| type | 用途 | protected | GC policy |
| --- | --- | --- | --- |
| manual | 明示 commit | true | none |
| auto | 自動 snapshot (取り込み完了時 = MVP / 定期 = Phase 4、§8) | false | shallow (個数 / 時間で tree を減衰) |
| repaired | repair 操作の中間 commit | false | shallow |
| purged | 法務・秘匿削除後の commit | true | none |

current reader は上記以外を拒否し、legacy enum の読取りや変換を行わない。

## 2.2 GC

> GC (§2.2-2.7) は Phase 4 で段階導入する ([09-mvp-scope.md §3.1](09-mvp-scope.md))。milestone 1 は read-only planner、milestone 2 は明示確認付きの receipt先行 tree-only shallow sweep、milestone 3 は bounded `after_index` hook、milestone 4 は OS scheduler から呼ぶ scheduled auto snapshot、milestone 5 は同じ `kio snapshot auto` invocation 内だけで動く Rust-only `on_idle` GC、milestone 8 は独立したRust-only unreachable-object read-only inventoryである。Kio は daemon / scheduler installer を持たない。

```text
gc_policy(commit_type):
  auto      → shallow   (tiered retention 満了で tree のみ破棄、commit object は残す)
  repaired  → shallow
  manual    → none
  purged    → none
```

**full (commit object の削除) はどの commit_type にも適用しない。** commit object は append-only であり、これを消す操作は Kio に存在しない (purge も commit / tree を書き換えない、§3.5)。

なお `kio repair verify-objects` ([10-operations.md §7.5](10-operations.md)) が生成する `repaired` commit は破損 object の再取り込みによる復旧点であり、その復元した raw object は GC 対象外 (§2.6)。したがって commit の tree が shallow 化されても復旧した raw 内容は保持され、object としては実効的に none 相当である。

`shallow` は履歴 DAG の連続性を保つため commit を残し tree のみ破棄する。実行時は
`(commit_hash, tree_hash, gc_policy, shallowed_at)` を持つ non-content receipt
(`.kio/gc/shallowed/<commit64>`) を**tree 破棄より先に耐久化する** (Phase 4 実装要件) — fsck は
receipt が説明する tree 欠落を正常 (shallow) として扱い、receipt なき欠落を corruption とする
([10-operations.md §7.5.1](10-operations.md)。これが無いと正規 GC と tree の偶発喪失を区別できない)。
milestone 2 の receipt は上記4 fieldだけの canonical JSON+LFであり、create-new後にfileと
`shallowed/` directoryをfsyncする。同名receiptはbyte/semantic完全一致だけを冪等成功とし、
unknown field、filename/hash/tree/time不一致、symlink/reparse、非regular、hardlinkはcorruptionである。

`shallow` 後の commit を対象に view した場合 (`kio view <path> --at <commit>` — 文法の正本は [06-cli-spec.md §1](06-cli-spec.md)。commit の metadata 表示は `kio log` / `kio inspect` 系が担う):

```text
- メタ情報 (commit_hash, parents, message, timestamp, commit_type) は表示
- tree は "shallow: tree discarded" と表示
- kio restore <shallow-commit> は KIO-E-COMMIT-SHALLOW-001 で拒否
- kio diff <a> <b> で片方が shallow なら全ファイル差分は不能と明示
- kio search --at <shallow-commit> と、shallow 化 commit を snapshot とする
  cursor の再計算も KIO-E-COMMIT-SHALLOW-001 で失敗する (tree 全体を要するため)
- shallow commit を指す Evidence Pointer の解決は失敗しない
  (raw_hash / chunk_hash による直接解決、08-evidence-pointer-spec.md §3.1)
```

## 2.3 GC スケジューリング

GC は独立した常駐プロセスを持たない (§5 プロセスモデル)。実行契機は次の 3 つ:

1. `manual_only` (**現行デフォルト**): `kio gc` の明示実行のみ
2. `after_index` (Phase 4 milestone 3、明示 opt-in): `kio index` / manual `kio snapshot create` の**成功かつ non-partial**な durable publication 後、既存 writer lock を解放してから同一プロセス内で `max_runtime_seconds` を soft upper bound として実行する。preview、usage error、失敗、partial index、`index --revoke-network` は発火点ではない
3. `on_idle` (Phase 4 milestone 5、明示 opt-in): OS scheduler が起動する indexed scope の `kio snapshot auto` (§8.2) だけが発火点である。eligible な idle 観測の後、snapshot writer の publication を完了して `.kio/.lock` を解放してから、GC 専用 bound lock と fresh plan の下で既存 receipt / index rotation / recovery / runtime budget 規約を実行する。`kio index`、manual `kio snapshot create`、preview、usage error、失敗、partial index、`after_index` は cofire しない

`after_index` は wall clock でなく process-local monotonic clockを使う。deadline は hook 開始（config / plan / recovery 検証を含む）から計測するが、tree / SQLite copy の途中を打ち切らず、次の**耐久済み checkpoint**で停止するため、個々の bounded operation に要した時間だけ soft bound を超え得る。checkpoint は marker publish、phase marker 交換、新規 receipt publish、pre-sweep index rotation の各耐久段階、tree 1件の退役完了である。final index rotation と marker 完了は反復 starvation を避けるため一つの不可分な完了単位として扱う。各 invocation は期限超過時でも最低1つの耐久 stepを完了してから停止し、同一の既存 receipt の再確認は予算を消費しない。

automatic activation は writer 開始前に capability-relative に検証した **`[gc]` 全体**（mode、runtime、retentionを含む）の canonical semantic digestと、retained scope / `.kio` directory identityへ固定する。index が自ら更新し得る adapter/network 設定はこの authority に含めない。durable publication 後とGC専用lock下のlocked re-plan前後で同じauthorityを要求し、差分があればtree/receipt/markerを変更せず `KIO-E-GC-CONFIG-CHANGED-001` / retryable exit 3 とする。publication 済みなら `publication_status=completed` を保持する。

期限到達は corruption ではなく `status=deferred` / `KIO-E-GC-RUNTIME-LIMIT-001` / retryable exit 3 であり、markerと完了済みreceipt/tree進捗を残す。次の同 mode の automatic writer入口（`after_index` は index/manual snapshot、`on_idle` は `snapshot auto`）は通常 writer lock より**前**にこの marker を同じbounded経路でresumeし、再び期限なら publication を開始せず `publication_status=not_started` で返す。post-publication sliceの期限・失敗は既にdurableなindex/snapshotをrollback扱いせず `publication_status=completed` とGC結果を同じpayloadに載せる。`manual_only` のactive markerは従来どおり通常writer barrierとなり、明示 `kio gc` でresumeする。

automatic mode は明示 opt-in である。`[gc]` 不在時だけ `manual_only` を既定とし、table がある場合は `mode` を必須とする。`manual_only` は runtime / idle を禁止、`after_index` は `max_runtime_seconds` (1..86400) を必須・idle を禁止、`on_idle` は同 runtime と `idle_threshold_seconds` (1..31536000) を両方必須とする。default を automatic mode へ変更しない。

```toml
[gc]
mode = "on_idle"               # Phase 4 milestone 5 の明示 opt-in
max_runtime_seconds = 60
idle_threshold_seconds = 300
```

## 2.4 Tiered Retention

`commit_type=auto` のみ tiered retention を適用する。retention 満了は **shallow 化 (tree 破棄)** であり commit object の削除ではない (`manual` / `purged` は tree も常に残す)。`repaired` は `[gc.derived_retention]` に従う — branch ごとに最新 `keep_repaired_per_branch` 個の tree を保持し、超過分を shallow 化する (ref tip は除外・tiered retention (auto) とは別系統)。
**ref tip 除外**: HEAD・branch・tag が指す commit の tree は、retention 満了でも **shallow 化の対象にしない** — 無変更 scope では auto snapshot が no-op を続け HEAD が古い auto commit に留まり続けるため、除外しないと現在状態の基点 (bare search / restore / cursor) を失う。物理削除の直前にも、ref tip 非該当と「非 shallow commit からの参照ゼロ」を同一 exclusive critical section で再検証する (§2.5):

```toml
[gc.auto_retention]
keep_last_hours    = 24
keep_hourly_days   = 7
keep_daily_weeks   = 4
keep_weekly_months = 6
[gc.derived_retention]
keep_repaired_per_branch = 5
```

## 2.5 並行性 / power-loss 安全性

```
- milestone 2 の on-demand sweep は preview/確認後から完了まで `.kio/.lock` をexclusive保持する。
  `.kio/gc/in_progress` が残る crash recovery 中も、通常writerは
  `KIO-E-GC-SWEEP-ACTIVE-001`で拒否し、GC resumeだけが専用lock入口を使う。新規commitをblockしない
  CoW型GCは後続milestoneであり、本実装には含めない
- milestone 3 の `after_index` は index/snapshot のdurable publicationとそのwriter lock解放後にGC専用lockを
  取得する。active markerは通常writer取得前にauto-resumeする。internal child scopeはchild process自身が1回だけ
  実行し、保持済みchild root/`.kio` capabilityと再bind先のidentityが一致しない限りfail-closedする。親hookが
  childや親以外のscopeへ代理適用されることはない
- markerはreceiptより先にatomic publishし、file/directory fsyncする。phaseは
  `prepared → receipting → sweeping → finalizing`。更新はstrict versioned markerを
  capability-relativeに交換し、operation/plan/truth/candidate/tree/index stateを固定する
- shared treeは対象commit全件のreceipt耐久化後に1回だけ除去する。除去直前にも全ref tip、全commitの
  tree共有、receipt、marker、bound scope identityを再検証し、相違は自動再計画せずfail-closedにする
- tree/marker の隔離・退役名は `.kio/gc/internal/` 配下の operation-reserved namespace とする。
  retained descriptor、nofollow、no-replace exchange、identity/hash/single-link の再検証で、隔離前および
  検出可能な隔離後の差替えは fail-closed にする。POSIX の最終 pathname unlink には inode 条件を付与
  できないため、検証直後の reserved name へ第三者が直接書き込む残余窓は §3.5 の restore 隔離と同じく
  保護契約外とする。この例外は public CAS path、scope/fanout directory、receipt/marker public name、
  hardlink、または unlink 後の retained handle に対する検証を緩めない
- **generation 採番の順序**: sweep は**最初の tree 物理削除に先立ち** `index_generation` を新規採番・
  耐久化し、**sweep 完了時にも再採番**する — sweep 前に発行された cursor は開始時採番で、sweep 中に
  発行された cursor は完了時採番で、いずれも generation 不一致として拒否される (§1.5)。途中 crash
  しても削除済み tree と旧 generation の組は観測されない (再開 sweep も完了時に再採番する)
- **sweep 実行中 (in_progress マーカー存在 — crash 残骸を含む) は新規 cursor を発行しない** (page 1 は
  cursor なし応答 + 注記 — sweep 中に発行した cursor が同一 generation のまま変化する stream へ
  consumed を適用する窓を作らない。replay は generation 検査で自然に拒否される)
- 各回転は公開 `index/sqlite.db` をin-place更新しない。retained `.kio` capabilityからsourceの
  generation/physical identityとsource file state（size/mtime/ctime）を固定し、`.kio/gc/internal/index/` の
  private copyを更新・file fsyncしてから、source state、source/target/private-directory identityをmarkerへ
  耐久化する。その後、descriptor-relative atomic exchange直前にもsource stateを再照合した上でexchangeを
  行い、公開`index/`、private directoryの順にfsyncする。recoveryは公開leafがmarkerのsourceかtargetである
  場合だけ再開し、同generationの別inode、private directory/leaf差替え、unsupported platformはreceipt publish前
  またはtree除去前にfail-closedとする
- private copy の pre-sweep generation 更新と同一 SQLite transaction で、strict singleton
  `gc_rotation_attestation`（version、sweep ID、role、plan digest、source/target generation）を記録する。
  tree除去直前に公開DBのgeneration/physical identityとこのattestationをmarkerへ再照合し、単にmarkerの
  `index_pre_sweep`を現在値へ偽装した状態を回転済みとは扱わない。coreのtree除去APIはこのtrusted index検証後に
  発行されるprocess-local permitを必須とし、marker JSONだけを物理削除authorityにしない
- power-loss 中断時は次回起動時に sweep 再開 (.kio/gc/in_progress マーカーで検出)
- markerだけでreceipt/tree進捗が無い場合に限り、locked fresh planとの不一致を確認してmarkerを退役し、
  新規previewを要求してよい。一度作成したreceiptはrollbackしない
```

## 2.6 GC の削除対象 (規範)

現行 retention GC が削除してよいもの:

```text
- tree object (shallow 化対象 commit のもの。**ただし同一 tree hash を非 shallow の commit が参照して
  いる場合は削除しない** — tree は content hash 共有されるため、reachability 確認 (全非 shallow commit
  からの参照 0) が削除の前提)
- sweep 前後の SQLite generation rotation は §2.5 の protocol に限る。index/FTS 全体の再構築は
  `kio repair rebuild-db` の責務であり、unreachable-object inventory は SQLite を変更しない
```

GC が削除してはならないもの:

```text
- commit object (append-only。§2.2)
- raw object / chunk object — これらを削除する唯一の経路は purge (§3)
- toollock object — 参照する commit object が存在する限り削除不可 (commit は append-only のため実質恒久。
  未公開 finalize 由来の未参照 toollock は milestone 8 で診断候補にできるが、削除はしない)
- manifest object — 参照する tree object が存在する限り削除不可 (削除の唯一の経路は purge。shallow 化で
  未参照になったものは milestone 8 で診断候補にできるが、現行 retention GC は回収しない)
- prepared / image object — lifecycle の削除主体は `kio repair verify-objects --prune-orphans` であり、
  unreachable-object inventory は削除候補にしない
```

raw / chunk を GC 対象外とするのは、Evidence Pointer の永続性契約 ([08-evidence-pointer-spec.md §6](08-evidence-pointer-spec.md)) を「purge されない限り」で成立させるため。ストレージ増は「原則として忘れない」設計の受容済みコスト。

on-demand tree-only shallow sweep は Phase 4 milestone 2、bounded `after_index` hook は milestone 3、scheduled auto snapshot は milestone 4、Rust-only `on_idle` GC は milestone 5 で実装済みである。本節の削除対象規範と §2.2 の gc_policy schema は Step 1 の DB / object 設計時から遵守する。

## 2.7 Unreachable-object inventory (Phase 4 milestone 8)

`kio gc --dry-run --prune-unreachable [--json]` は Rust-only・read-only の物理 CAS inventory である。
名称に `prune` を含むが、削除、truncate、隔離、receipt、marker、resume、確認prompt、`--yes`、
自動hookは持たない。過去のreportを含め、出力はmutation authorityにならない。

到達性は SQLite を根拠にせず、全 ref root と append-only commit DAG、物理tree、immutable manifest →
normalized-unit pin、commit → immutable tool-lock、embedding → chunk text/image target、strict shallow receipt、
canonical purge lifecycleから導出する。refから未到達のcommitもappend-onlyであり、そのcommitが参照する
tool-lockとtree closureを誤回収しない。完了済みpurgeが説明するhistorical closure欠落は正常であるが、
markerの構造・`in_commit`・`purged_raws` membership・ref到達性・時刻・resurrection ancestryのいずれかを
検証できなければ説明に使わずfail-closedする。欠落を説明できるtreeはfinal purge/resurrection anchorの
strict ancestor commitが参照するものだけであり、anchor以後や無関係branchへ説明権限を拡張しない。

| kind | classification / reason |
| --- | --- |
| commit | `protected / append_only_history`、ref未到達でも `append_only_unreachable_history`、shallow receipt境界は `append_only_shallow_history` |
| tree | `protected / retention_gc_owned`。削除判断は既存retention GCだけが担う |
| raw / chunk | `protected / evidence_pointer_permanence`。物理削除経路はpurgeだけ |
| manifest | 物理treeから参照ありは `protected / tree_referenced`、参照ゼロを証明した場合だけ `candidate / zero_tree_references`。validなshallow receiptが一つでもあり欠落treeのclosureを証明不能なら `inventory_only / shallow_history_unavailable` |
| normalized unit | manifest pinありは `protected / manifest_referenced`、pinゼロを証明した場合だけ `candidate / zero_manifest_references`。purgeで欠落したhistorical manifestとraw/profile/genが一致してpinを証明不能なら `inventory_only / historical_manifest_unavailable`、validなshallow receiptが一つでもあり欠落tree/manifest closureを証明不能なら `inventory_only / shallow_history_unavailable` |
| embedding | verified chunk text/image targetありは `protected / target_referenced`、正当なtarget kind/hashで対象ゼロを証明した場合だけ `candidate / zero_target_references`。targetを証明不能なら `inventory_only / unprovable_target` |
| tool-lock | commit参照ありは `protected / commit_referenced`、全commit参照ゼロを証明した未公開objectだけ `candidate / zero_commit_references` |
| prepared / image | `inventory_only / verify_objects_orphan_lifecycle`。`repair verify-objects --prune-orphans`の専管 |

走査は retained scope / `.kio` descriptor と同じwriter gateのshared lockを保持し、すべて capability-relative・
nofollow で行う。symlink/reparse、非regular、unsafe hardlink、identity/size/hash/link countの変化を拒否する。
このmutation-free shared descriptor gateを実装済みのplatformはmacOS / Linuxであり、その他ではscopeを
変更せずfail-closedする。
object数、総physical/verified bytes、manifest unit数、history/ref/receipt/directory/name/depthを上限化し、
refs・receipt・purge/GC stateを含む独立した全走査を2回行って結果とfilesystem observationの完全一致を
要求する。active/recovery/uncertain state、欠落、malformed、race、上限超過はcandidate 0へ縮退せず
fail-closedし、failure時に部分stdoutを出さない。report schemaは [06-cli-spec.md §6.2](06-cli-spec.md)、
object graphは [03-data-model.md §8.3](03-data-model.md) を正本とする。

# 3. Purge (法務・秘匿・誤取り込み)

## 3.1 purge と archive の区別

```
archive: 履歴上は残し「現在は使っていない」状態。デフォルト操作。
purge:   履歴から物理的に消す。例外操作。commit_type=purged が記録される。
```

正当事由:

```
- 法令上の削除義務 (個人情報・GDPR の forget 権)
- 機密漏洩への対応 (誤って取り込んだ秘匿文書)
- 著作権・契約上の保持禁止
- 誤取り込みの是正 (取り込むべきでなかった対象 — 秘匿文書に限らない)
```

CLI:

```bash
kio purge <path|--raw-hash <h>> --reason <legal|privacy|misingest|copyright|other>
# --reason は必須。--yes なしなら確認プロンプト
```

## 3.2 「忘れない」と purge の両立

Kio は「原則として忘れない」が、**purge は「忘れる」のではなく「消した事実を記録して忘れる」操作**。purge 後も:

```
- commit_type = "purged" の新 commit が記録される
- 誰が、いつ、どの正当事由で実行したかを保存
- 監査可能性は維持される (= 透明な忘却)
```

## 3.3 Dead Evidence Pointer のセマンティクス

「Evidence Pointer の不変性」と「法務 purge」の緊張領域。正本は [08-evidence-pointer-spec.md §4](08-evidence-pointer-spec.md)。以下は採用済みセマンティクスの要約。

```text
purge 後の pointer 解決:
1. raw_hash の canonical final event が `purged` (全 marker 正本化 — 08 §3.1 手順 5) → tombstone レスポンス (status = tombstoned)
   {
     "status": "tombstoned",
     "purged_at": "2026-04-25T12:00:00Z",
     "purged_reason": "legal" | "privacy" | "misingest" | ...  (enum の正本 = 08 §4.1),
     "purged_in_commit": "sha256:9f2c...",
     "raw_hash": "sha256:..."
   }
2. raw_hash が完全削除 (--erase-tombstone: public tombstone 記録を残さない) → not_found
   error_code: KIO-E-PURGE-NOT-FOUND-001

検出 API:
kio evidence verify <pointer> [--strict]
  → status = 6 値 union (alive | tombstoned | not_found | scope_unreachable |
             unverifiable | registry_duplicate — 正本 08 §4.3)
```

## 3.4 purge スコープは `.kio` 単位

横断 GC を持たないので、purge も **その `.kio` 内に閉じる**。別 `.kio` (= ユーザーが意図的に複数フォルダへ配置) に同一 raw_hash がある場合、それは別 purge 操作で消す必要がある。

## 3.5 purge の機構 (何を消し、何を残すか)

purge は **object の物理削除 + default tombstone または内部 erase receipt** であり、
**履歴 DAG の書き換えではない**。

消すもの (対象 raw_hash について、全履歴にわたり):

```text
- raw object 本体 (objects/raw/ab/cd/<raw64>)
- 派生 artifact: prepared / **image** / normalized / chunk / embedding
  (normalized は同一 (raw_hash, tool_profile_hash) 配下の全 gen instance を対象とし、
   **manifest object (objects/manifests/ — 当該 (raw_hash, tool_profile_hash) の全 gen・全確定版) を含む**。
   **共有されうる派生 (prepared / image — content hash 単位で他 raw と共有され得る ([03-data-model.md §1](03-data-model.md)) / embedding — text_hash 単位で他 raw の chunk と共有) は、purge 対象外の
   live 参照が 0 の場合のみ物理削除する** — 無条件削除は非対象文書の検索・再構築を破壊する)
- `~/.cache/kio/open/<raw_hash digest64>/` の一時展開 dir (存在すれば冪等削除 — [06-cli-spec.md §1.1](06-cli-spec.md))。
  **本 closure で物理削除対象となった image (live 参照 0)** の一時展開 dir
  `~/.cache/kio/open/image/<image_hash digest64>/` ([06-cli-spec.md §1.1](06-cli-spec.md) — `image/` の
  type segment で raw 系 dir と分離) も同様に冪等削除する
  (live 参照が残る共有 image の cache dir は削除しない — 当該 raw に帰属しない)
  (closure の列挙正本 = 当該 (raw_hash, tool_profile_hash) の全 gen manifest。**どの manifest からも
   参照されない orphan prepared / image** (公開前 crash の残骸) は解決経路に乗らず、GC の
   「未参照中間 object」として回収される。**MVP では GC が無いため、削除手段は
   `kio repair verify-objects --prune-orphans`** ([10-operations.md §7.5.1](10-operations.md)) —
   purge 完了表示にその旨 (残存可能性と掃除手段) を注記する)
- SQLite の chunks / chunk_config_generations / chunk_publications 行と FTS エントリ。chunk_vec は**対象 chunk_id の行に限定**し、**embeddings 行は object 側と同じく live 参照 0 の場合のみ削除する** (共有 text_hash の行を無条件に消すと、非対象文書の vector 検索が rebuild まで欠ける)。cursor replay の query-vector cache は source SQLite 行ではなく文書 lifecycle と無関係な device-local file なので、この purge closure の対象に含めない (§1.5)。**`image_vec` 行と `target_type='image'` の embeddings 行も同じ規則で列挙する** (2026-07-26 — [04-pipeline.md §4.3](04-pipeline.md) の `image_vec` 新設に対応。判定単位は `image_hash`。上の bullet が image object そのものを「live 参照が残る共有 image は削除しない」としているのと同じく、**共有画像のベクトルも live 参照 0 の場合のみ削除する** — 同一の画像が非対象文書からも参照されている場合に消すと、その文書の画像検索が rebuild まで欠ける)
- chunks.jsonl の**対象 chunk_id を参照する creation 行・publication event 行の全部** (append-only の例外 — purge は法務要件の明示例外として行を落とす。書き換えは [04-pipeline.md §1.1](04-pipeline.md) の耐久書込 primitive (temp + rename) に従う)
- 対象 raw_hash に帰属する task の **staging** ([07-adapter-spec.md §8.3](07-adapter-spec.md)) — **task の状態を問わず** (retryable failed の保全 staging を含む。以後の再生成は persist 直前の tombstone 再検査が防ぐ)。**帰属列挙の正本 = `.kio/staging/` の耐久 descriptor 全走査** ([03-data-model.md §2](03-data-model.md) — tasks.jsonl 非依存。task 記録の喪失後も削除対象を列挙できる)
- **device replica (`~/.cache/kio/aggregator.sqlite`) の当該 scope の投影** — purge 成功時に
  scope 全体を再射影して置き換える (§1.8 write-through)。**replica は chunk 本文を持つ**ので、
  読み手任せにすると「誰も検索しない間、purge した本文が device の cache に読める形で残る」。
  順位の正しさの話ではない（通常 writer の射影失敗なら `Rebuilding` marker が検索を止める）— **本文を消すのが
  purge の目的そのもの**だという話である。
  **この再射影に失敗したら purge は成功を報告しない** (`KIO-E-PURGE-REPLICA-001` / exit 1、R25-5)。
  通常 writer は cache の射影失敗を `Rebuilding` にして source 側の成功を維持できるが、purge はその
  一般則の**唯一の例外**である。ここでの「cache」は、ユーザーが消滅させるよう求めた本文の、
  この device 上の第 2 の複製だからで、それが読める状態で成功と告げるのは劣化した結果ではなく
  **誤った結果**である。復旧手段 (purge 再実行 / cache root の `aggregator.sqlite` 削除) を
  message に含める
```

残すもの (不変):

```text
- すべての commit / tree object。commit / tree は書き換えない。
  DAG の再結線・tree entry の削除・連鎖再 hash は行わない。
- tree entry のメタデータ (path, raw_hash)。raw_hash から原文は復元できない。
- tombstone (.kio/tombstones/ab/cd/<raw64>)。--erase-tombstone 指定時を除く。
- `--erase-tombstone` では non-public の non-content erase receipt
  (`.kio/purge/erase-receipts/ab/cd/<raw64>`)。public tombstone としては不可視 — 用途は
  fsck の欠落説明・08 §3.1 手順 5 (ii)〜(iii) の not_found 分類・6b・resurrection link・
  同一 marker 自身の lifecycle 管理 (retired / 再 erased の append) に限る
  ([08-evidence-pointer-spec.md §4.2](08-evidence-pointer-spec.md))。
```

追加されるもの:

```text
- commit_type=purged の新 commit (purge 実行後の working tree を指す)
```

**working tree の原本には触れない** (Kio はユーザーのファイルを削除しない)。したがって purge の
preview と完了表示は、対象 raw_hash と同一 bytes の原本が working tree に残存する場合に**必ず警告する**:
残存原本は次回 `kio index` の自動 scan で再取り込みされ、既存 pointer は再び alive になる
([08-evidence-pointer-spec.md §4.2](08-evidence-pointer-spec.md))。恒久的に除外するには原本の削除または
`.kioignore` への追加が必要である。

**tombstone の退役 (resurrection)**: 同一 raw_hash の raw object が再 publication された場合、その
publication と同一の locked mutation 内で active tombstone を**退役 (retire)** させる — tombstone
レコードの events[] へ `retired` を append する (下記 lifecycle 形式)。**耐久順序**: retire の
append は再 publication の snapshot finalize (§8.1 — chunks.jsonl → SQLite → commit / ref publish)
の**完了後**に行う。間で crash した場合は tombstone が active のまま残る (安全側 — 解決は
tombstoned)。retire append の完了時に index_generation を新規採番する (§1.5 — finalize〜retire 間に
発行された cursor の replay が、退役後の可視集合で別 stream を再計算することを拒否で防ぐ)。
回転は retire append と同一 locked mutation 内で直後に行う。lifecycle 更新の検出は**時刻ではなく
単調カウンタ**で行う: `.kio/tombstones/lifecycle-epoch` (`.kio/purge/epoch` と同じ書込規律の単調
カウンタ) を **event append (retire・再 purge) ごとに同一 lock 下で +1** し、回転の
SQLite Tx は index_metadata の **`last_lifecycle_epoch`** ([04-pipeline.md §4.1](04-pipeline.md)) へ
反映済み counter 値を記録する。append と回転の間で crash した場合は、書き込み系コマンド冒頭の
回復が **counter > last_lifecycle_epoch** を検出して回転を補完する (UTC ms の時刻比較は同一ミリ秒・
時計逆行で補完を見逃すため使わない。`kio repair rebuild-db` は完了 Tx で現 counter 値に初期化する
— DEFAULT 0 のままの全件誤検出を防ぐ)。**counter の耐久順序と回復**: counter の +1 (fsync) を
event append より先に行い、**全ての lifecycle event (purged・erased・retired) に、その時点の
counter 値を `lifecycle_epoch` として必須記録する** (purge の `epoch`
(target_epoch) とは**別 field** — 2 系統のカウンタを混用しない)。各 marker record 内では
`lifecycle_epoch` を正の値かつ event 順に**厳密単調増加**とし、重複・逆行は読取時に
`KIO-E-STORE-CORRUPT-001` として fail-closed する。
**巻き戻り検出は機械条件のみ**: locked mutation 冒頭で
`counter < max(last_lifecycle_epoch, 全 lifecycle event の lifecycle_epoch 最大値)` (lifecycle_epoch を記録した event が無ければ後者は 0 として評価) なら欠落・不正・
backup 復元による巻き戻りとみなし、**その max + 1 で counter を再作成して無条件で
index_generation を 1 回転する** (取りこぼした可能性のある更新を回転で潰す fail-safe。
「更新痕跡」の判定はこの比較だけで行い、mtime 等の抽象的条件は使わない)。**読取系は冒頭検査で
counter と last_lifecycle_epoch を照合し、不一致 (> だけでなく < も) なら
KIO-E-INDEX-REBUILDING-001 と同じ retryable (exit 3) を返す** — 補完回転は書き込み系のみが行うため、
crash 後最初のコマンドが読取でも旧 cursor を退役後の可視集合へ受理しない (この retryable への自動再試行は仕様として約束しない — 再試行は呼出側の判断)。
次回の locked mutation または fsck が「canonical final event が `purged` のままの tombstone × **verified raw (content hash 検証済み) の存在** × 同一 raw の ref 到達可能な
再 publication commit **であって、canonical final purged event の `in_commit` を ancestor に持つもの
(= 当該 purge より後の publication)**」を検出したら retired event を補完する (raw が欠落・破損のままの
補完は不可 — tombstone を誤って退役させない) (erase receipt の
crash 整合規則と同型。補完条件の正本は [10-operations.md §7.5.1](10-operations.md) — **この因果条件を
満たす再 publication commit が無い共存は incomplete purge (exit 3) であり補完しない**。**この因果条件が無いと、再 purge 後も ref に残る過去の resurrection commit を
誤検出して、新しい tombstone を退役させてしまう**)。以後の
open / view / verify / 解決は alive を返す (退役なしには「tombstone 最優先」の解決規則と上記の
「再び alive」が両立しない)。**retired event には `resurrection_commit` (再 publication を刻んだ
commit — §8.1 no-op 例外 (a)) を記録する** — purge 前 commit を指す旧 pointer の解決は、このリンクを
介してのみ新 publication を参照できる ([08-evidence-pointer-spec.md §3.1](08-evidence-pointer-spec.md)
手順 6b。検索の時点条件には影響せず、旧時点への遡及混入は起きない)。purge の監査事実は
commit_type=purged の commit と、削除されず残る purged/retired event 列で追跡できる。
search / open / evidence verify (resolver 系) は [08-evidence-pointer-spec.md §3.1](08-evidence-pointer-spec.md) 手順 5 の **canonical final event 判定**を共有する。fsck・再 purge (marker lifecycle 管理) は各 marker の末尾 event 規則によるが、**raw 不変条件・修復の判定だけは canonical を基準にする** ([10-operations.md §7.5.1](10-operations.md))。

**purge journal (クラッシュ安全の正本)**: purge は複数ストア (objects / SQLite / chunks.jsonl / logs /
tombstone / commit) を跨ぐ破壊操作のため、**mutation 前に `.kio/purge/journal` へ対象 closure と
phase を耐久記録し (fsync + atomic rename — [04-pipeline.md §1.1](04-pipeline.md) と同じ書込規律)、
各 phase を冪等に再開できる**ようにする:

```text
journal record = { purge_id (ULID), raw_hash 群, reason, actor, started_at, target_epoch (完了時の epoch 値),
                   marker_kind (tombstone | erase),
                   closure (削除対象の全 object type × hash — 共有派生の live 参照判定の結果を含む),
                   planned_commit (purged commit の canonical bytes — prepared 相で確定し、
                                   tombstone / receipt の purged / erased event の in_commit と
                                   一致する hash を先に固定。`purged_raws` = 対象 raw_hash 昇順配列を
                                   必須 field に持つ — [03-data-model.md §8](03-data-model.md)。
                                   marker の purged / erased event の `at` は planned_commit の
                                   `created_at` と同一値 — prepared 相で確定した単一 timestamp) }
phase 順序    = prepared (closure 確定・記帳)
              → tombstoned (tombstone / erase receipt を先に耐久化 — 削除より前)
              → deleted (objects / SQLite / chunks.jsonl / logs の冪等削除)
              → committed (commit_type=purged の publication)
              → done: **順序固定** — (1) `.kio/purge/epoch` を journal の target_epoch へ更新
                (temp 書込 → file fsync → atomic rename → 親 directory fsync)、(2) その後に
                journal を除去 + directory fsync。journal が先に消える実装は、除去〜increment 間の
                crash で「journal 不在 × 旧 epoch」の ABA 窓を作るため禁止。
                **順序の固定は全 platform の義務だが、「親 directory fsync」は POSIX のみの手段である**
                — 正本は [04-pipeline.md §1.1](04-pipeline.md) の同名の注記で、purge journal に
                固有の話ではなく objects/ の CAS leaf も同じ `kio-core::purge::sync_directory` を
                共有する。ここでの帰結は、Windows では「journal を除去した」という**主張の強さ**が
                NTFS の metadata journalling 頼みになる一方、除去〜epoch increment の順序 (上記の
                ABA 窓を閉じる本体) は変わらないこと。fail-closed 側 (親が不在または directory で
                ない場合に呼出元へ surface する) は両 platform で保たれる
クラッシュ回復 = 次回の書き込み系コマンド冒頭で journal を検出したら、記録 phase から再開する
              (各 phase は再実行安全 — planned_commit を journal から publish するため同一 hash を
              再現でき、時刻の再計算をしない)。journal が active な間の fsck は incomplete (exit 3 —
              [10-operations.md §7.5.1](10-operations.md))。**読み取り系 (status を除く §6 の全読取
              コマンド — search / log / view / inspect / evidence verify / restore / diff / open) は、
              冒頭と「本文・存在情報を返す直前」の 2 点で検査する: 「active journal の不在 **かつ**
              `.kio/purge/epoch` (単調カウンタ) が開始時と不変」でなければ `KIO-E-PURGE-JOURNAL-ACTIVE-001`
              ([10-operations.md §11.1](10-operations.md)) retryable (exit 3) で拒否する** (2 点目で検出した場合は取得済み結果を破棄する。
              epoch 比較が無いと、高速な purge が 2 点の間に journal 作成〜除去まで完走した場合に
              両検査をすり抜ける — ABA。**epoch ファイルの欠落・不正値も同様に拒否する (fail-closed)** —
              次の locked mutation が journal の target_epoch、journal も無ければ**全 lifecycle
              event に記録された `epoch` の最大値 + 1** (`epoch` を記録した event が皆無なら 1 = event ゼロの store。旧観測値と衝突しない)
              から単調性を回復して再作成する。purge 完了後に epoch ファイルだけ喪失しても恒久
              exit 3 にしない) —
              marker 耐久化後・削除完了前の窓で削除対象の本文を返さないため。読み取り系は lock を
              取らないため、冒頭 1 回の検査では検査後に journal が現れる TOCTOU 窓が残る — 返却直前の
              再検査 (journal / purge epoch / **lifecycle counter** の 3 点 — 順序と比較対象は
              [10-operations.md §3](10-operations.md) の固定順) がこれを閉じる。`kio status` だけは拒否せず、active journal の存在を状態として
              表示する (クラッシュした purge の回復可視性のため。status は本文を返さない)。
              不可逆な外部副作用を持つ 2 系は検査位置を固定する: restore は private temp へ展開し
              返却直前検査の後に atomic rename で --to へ publish (検出時は temp を削除)。**出力名・上書き
              対象名が `.kio-restore-bak` / `.kio-restore-quarantine` で終わる場合は mutation 前に明示
              拒否する** (commit 展開では全出力 path を publish 前に検査。退避・隔離名前空間の予約 —
              残存退避を正規対象として再退避・cleanup すると先行 restore の回復コピーを失う。真にその名の
              ファイルを復元する場合は改名復元を案内)。**出力先の退避名 `<basename>.kio-restore-bak`・隔離名 `<basename>.kio-restore-quarantine`
              に同名ファイルが既に存在する場合は、--force の有無・宛先の存否に関わらず先行 restore の
              未完残存として mutation 前に拒否し、回復手順 (内容確認の上での手動復帰または削除) を
              案内する** (bak / quarantine とも出力 path ごとの mutation 前検査 — --force 限定にすると
              先行 crash で宛先が消えた後の非 --force 再実行が stale 退避を素通しする) (crash 残存の隔離物が purge 済み
              内容の生存コピーか第三者ファイルかは機械判別できない。隔離・退避は `--to` 配下のユーザー
              領域であり [04-pipeline.md §1.1](04-pipeline.md) の temp 掃除の対象外 — Kio は自動削除
              しない)。
              **publish の rename は非 --force・--force とも no-replace 相当 (RENAME_NOREPLACE 等) で行い、
              競合検出時は無変更で失敗する** (非 --force = preflight の不存在判定後に現れた第三者ファイルを
              無断置換しない。--force = 下記の退避が destination を空けた直後に現れた第三者ファイルを
              置換しない — この競合時は退避を元 path へ復帰 (下記の隔離検証方式) して終端する。意図的置換は
              退避 rename だけが担う。restore の競合終端は全て **KIO-E-COMMIT-RESTORE-CONFLICT-001
              (retryable exit 3 — context に閉 enum `conflict_kind` (publish_race /
              quarantine_rename_race / quarantine_mismatch / backup_mismatch / restore_rename_race /
              stale_backup / stale_quarantine) と `retry_disposition` (**transient = publish_race のみ** —
              transient は「次回 preflight を妨げる残存物を作らない競合」に限る。他は全て manual_action。
              自動再試行が安全なのは transient のみ)、および両者の所在)**)。**--force 上書き時は publish の rename に先立ち、既存
              ファイルを同一 directory 内の退避名 `<basename>.kio-restore-bak` へ no-replace rename で
              保全し、退避名を stderr に表示して退避の dev/inode を記録する** (置換 rename は旧内容を破壊
              するため、保全なしには下記の巻き戻しが原状回復にならない。**同名の退避が既に存在する場合は
              先行 restore の未完残存として拒否し、回復手順 (退避の手動復帰または削除) を案内する** —
              残存退避はユーザー領域のファイルであり Kio は自動削除しない。crash で退避だけが残っても、
              次回の同 path への --force restore がこの拒否で検出・案内する)。**restore はさらに rename
              完了後に同 3 点を再検査し、変化を検出したら対象 raw の canonical 状態
              ([08-evidence-pointer-spec.md §3.1](08-evidence-pointer-spec.md) 手順 5) を
              再解決する — 対象が alive のまま (無関係な lifecycle 変化) なら publish を維持して成功。
              対象 raw を closure に含む active journal を検出した場合は下記と同様に巻き戻して
              KIO-E-PURGE-JOURNAL-ACTIVE-001 (retryable exit 3) で終端する (返却直前検査の fail-closed と
              対称 — purge 意図の耐久化以後は提供しない)。対象の purge が完遂していた場合は巻き戻す:
              **publish 済みファイルは unlink せず、同一 directory 内の決定的隔離名
              `<basename>.kio-restore-quarantine` への no-replace rename で隔離し (隔離名は stderr に
              表示)、rename した実体を fstat の dev/inode 対照で自らの publish と検証する** (対照→削除の
              2 操作では対照後の置換窓が残るため、rename した実体の上で検証する。一致 = 隔離分を削除
              (**削除は pathname に対する操作であり、fstat〜削除間の隔離名への第三者置換は検出できない —
              操作中の予約名前空間への第三者書込は保護契約外とし、この残余窓は許容する** (並行 reader の
              既 open fd と同格の残余。POSIX に identity 束縛の削除は存在しない))。
              不一致 = 第三者ファイル — 元 path へ no-replace rename で復帰を試み、成功・失敗いずれも
              競合終端 (失敗時は隔離名のまま残す))。**退避の復帰・除去も同じ隔離検証方式で行う** (退避
              path 上の対照→rename / unlink は同型の置換窓を残す。隔離名は同時に 1 実体 — publish の
              隔離を処置してから退避の処置に再利用する): 退避を隔離名へ no-replace rename → rename した
              実体を記録済み dev/inode と対照 → 一致 = 復帰なら元 path へ no-replace rename・除去なら
              削除。不一致 = 第三者による退避差し替え — 退避名へ no-replace で戻し (失敗 = 隔離名の
              まま)、それ以上触れずに競合終端。**復帰後は preflight と同一の応答で終端する: canonical =
              `purged` (tombstone) なら tombstone、`erased` なら KIO-E-PURGE-NOT-FOUND-001** (競合処置は
              段階別 — --force publish の no-replace 競合 = 退避を復帰して終端 / **隔離 rename (publish
              巻き戻し・退避処置とも) の no-replace 失敗 = preflight 後に隔離名へ現れた第三者ファイル —
              双方不触で終端 (quarantine_rename_race)** / 隔離実体の対照不一致 =
              復帰を試みて終端 / 退避の対照不一致・復帰 rename の no-replace 失敗 = 不触で終端。いずれも
              両者の所在 (隔離名・退避名を含む) を表示して RESTORE-CONFLICT で終端する — 窓内の第三者
              置換を消さない。crash で隔離だけが残っても、次回の同 path への restore が同名残存の拒否で
              検出・案内する。巻き戻しにより publish の事後取消が --force 上書きを含めて成立 —
              lock 非取得のまま残余窓を閉じる。purge closure を Kio 自身が破らない)。open は
              OS アプリ起動の直前 (一時展開の cache publish 後) に再検査する (起動後は取消不能 —
              検査はそこまでに完了させる。拒否時は当該一時展開 — publish 済み cache を含む — を
              dev/inode 対照の上で除去して終端する。[06-cli-spec.md §1](06-cli-spec.md))
```

**in-flight 外部実行との整合**: prepared 相で、**当該 scope (purge を実行する `.kio` の scope_id) の**
対象 raw_hash を入力とする pending / running の外部実行タスク (batch_requests state 0/1 —
`request_kind` = batch / sync の両方。表はデバイスグローバルのため、scope_id 条件が無いと同一 raw を
持つ**別 scope** の実行中 request まで terminal 化・掃除してしまう — purge は `.kio` 単位) を
abandon 相当で terminal 化し (estimated 記帳 — [04-pipeline.md §5.8](04-pipeline.md))、
provider 上の対応 upload (batch 行のみ) を掃除する。**加えて、対象 raw_hash の terminal だが
`intent_token IS NOT NULL` の行 (残骸掃除未完 — [04-pipeline.md §5.8](04-pipeline.md)) の provider
残骸掃除も同じ prepared 相で完遂する** (これが無いと terminal 化直後の crash が残した機密 upload が
次の batch 系実行まで provider 上に残る)。purge 後に相 3 collect が出力を得た場合は、persist 直前の
tombstone 再検査で破棄する ([04-pipeline.md §5.8](04-pipeline.md) 相 3)。

tombstone を削除より先に耐久化するのは、「対象 object が消えたのに purge の痕跡が無い」状態
(corruption と区別不能な markerless absence) を作らないためである。

tombstone は raw_hash をキーとする **lifecycle レコード** (append-only の events[] 配列) で、CAS object ではないため `objects/` の外に置く。event は `purged` / `retired` の 2 種で、**active 判定 = 末尾 event が `purged` であること** — retire は末尾に `retired` を append し (上書き・削除しない = 退役監査の保全)、再 purge はさらに `purged` を append する。fsck・再 purge (marker 自身の lifecycle 管理) はこの「末尾 event」規則を参照する。**pointer 解決 (resolver) は、tombstone と erase receipt が併存する場合、各 marker の末尾 event を [08-evidence-pointer-spec.md §3.1](08-evidence-pointer-spec.md) 手順 5 の canonical final event へ正本化してから評価する** (lifecycle_epoch 最大・同値は tombstone 優先 — 個別 marker の active 判定だけで短絡しない)。`events[]` を持たない record は形式違反であり、読取・変換いずれの対象でもない (下記 malformed と同じ `KIO-E-STORE-CORRUPT-001`)。`reason` は 5 値 enum ([08-evidence-pointer-spec.md §4.1](08-evidence-pointer-spec.md)) に閉じており、enum 外の値は読取時点で corruption とする。**lifecycle レコードの更新 (retire・再 purge) は `.kio/.lock` 下で、temp 書込 → file fsync → atomic rename → 親 directory fsync で行う** ([04-pipeline.md §1.1](04-pipeline.md) と同じ規律)。malformed・途中破損 (torn JSON) の record は `KIO-E-STORE-CORRUPT-001` として fail-closed に扱う。**validity は receipt (後述) と対称に semantic 検証まで要求する** — purged event の `in_commit` が bounded verified CAS で ref-reachable な `commit_type=purged` commit を指し、当該 commit の `purged_raws` に marker の raw_hash が含まれ、`at` が canonical UTC かつ commit `created_at` と一致し、invocation の fixed now より未来でないこと (erased 側 (下記 receipt) および [10-operations.md §7.5.1](10-operations.md) と同一。kind 別必須 field・遷移文法の正本は [10-operations.md §7.5.1](10-operations.md))。**検証失敗の marker は入口を問わず (fsck・resolver・再 purge) 説明能力を持たない corruption (`KIO-E-STORE-CORRUPT-001`) とする**。
物理 leaf の `<raw64>` は論理 `raw_hash` から `sha256:` を除いた 64 文字の小文字 hex であり、
JSON 内の `raw_hash` は完全な `sha256:<64hex>` を保持する。物理 leaf は digest-only 名の 1 表現のみ
([03-data-model.md §2](03-data-model.md))。

```json
{
  "raw_hash": "sha256:abc...",
  "events": [
    { "kind": "purged",  "at": "2026-04-25T12:00:00Z", "reason": "legal", "actor": "user",
      "in_commit": "sha256:9f2c...", "epoch": 12, "lifecycle_epoch": 41 },
    { "kind": "retired", "at": "2026-05-01T09:00:00Z", "actor": "user",
      "in_commit": "sha256:1a2b...", "resurrection_commit": "sha256:1a2b...", "lifecycle_epoch": 42 }
  ]
}
```

`--erase-tombstone` は public tombstone を残さない一方、markerless absence と後発 store corruption を
fsck が区別できるよう、同じ digest-only fan-out に次の exact bounded receipt を atomically 保存する。

```json
{
  "schema_version": 2,
  "raw_hash": "sha256:abc...",
  "events": [
    { "kind": "erased",  "at": "2026-04-25T12:00:00Z", "in_commit": "sha256:9f2c...",
      "actor": "user", "reason": "privacy", "epoch": 12, "lifecycle_epoch": 41 },
    { "kind": "retired", "at": "2026-05-01T09:00:00Z", "actor": "user",
      "in_commit": "sha256:1a2b...", "resurrection_commit": "sha256:1a2b...", "lifecycle_epoch": 42 }
  ]
}
```

receipt は path / query / prompt / content を持たず (actor は全 event、**reason (5 値 enum — 非機微 metadata) は purged / erased event** に監査要件として持つ — [02-philosophy.md §2.4](02-philosophy.md) の「どの正当事由で実行したか」を erase 後も保存する。kind 別の必須列挙は [10-operations.md §7.5.1](10-operations.md))、raw_hash は immutable tree に既に残る。**purged / erased
event には当該 purge の `target_epoch` を `epoch` として記録する** (全 event で必須。
epoch ファイル喪失時の回復源 — 上記 journal 二重検査の回復規則)。
validity は leaf/raw_hash 一致だけでなく、erased event の `in_commit` が bounded verified CAS 上で
ref-reachable な `commit_type=purged` commit を指し、当該 commit の `purged_raws` に対象 raw_hash が
含まれ、`at` が canonical UTC かつ commit `created_at` と一致し、invocation の fixed now より未来でないことを要求する。schema_version ごとの定義に
一致しない field・不一致は store corruption (`events[]` を持たない形式は tombstone と同型に
形式違反であり、変換対象ではない)。
re-ingest barrier・public tombstone 判定には使わない。使用できるのは fsck の intentional absence
説明と、pointer 解決内部の not_found 分類 (08 §3.1 手順 5 (ii)〜(iii))・手順 6b の欠落説明・
resurrection link・同一 marker 自身の lifecycle 管理 (retired / 再 erased の append — 本節)
に限る ([08-evidence-pointer-spec.md §4.2](08-evidence-pointer-spec.md) の列挙が正本)。
したがって Evidence verify の応答は従来どおり `not_found` で、同一 bytes の
後日 ingest (明示操作に限らず、working tree 残存原本の自動 scan を含む — §3.5 の残存警告) は許可する。**erase receipt も tombstone と同じ lifecycle 形式 (events[]) を持ち、raw object の再 publication 成功時は除去せず `retired` event を append する** — 除去すると erase 済み raw の旧 commit が参照する manifest 欠落を説明するものが消え、fsck の corruption 誤判定と手順 6b の不達を生むため (公開 pointer API に使わない・re-ingest barrier にしない性質は不変)。crash で不整合が残った場合は verified raw object を優先し、次の locked mutation で record を整合させる — **整合の条件は [10-operations.md §7.5.1](10-operations.md) の receipt 整合規則に従う**: 当該 erased event が全 marker の canonical final event ([08-evidence-pointer-spec.md §3.1](08-evidence-pointer-spec.md) 手順 5) であり、**canonical final event の** `in_commit` を ancestor に持つ ref 到達可能な再 publication commit が存在するときのみ `retired` を append する。canonical が別 marker の `purged` なら incomplete purge として exit 3 で報告する (append しない)。commit がまだ無ければ未 finalize の進行状態として保留する (tombstone の補完と同じ因果条件)。

**制約 (明記)**: tree entry の `path` 文字列と `raw_hash` は履歴に残る。ファイル名そのものが秘匿対象であるケース (履歴書き換えが必要) は current purge の対応範囲外である。

# 4. Restore / Time-travel

## 4.1 Restore

```bash
kio restore <evidence|path|commit> --to <dir>
```

**安全要件**:

```
- working tree への直接書き戻しは禁止 (--to <dir> 必須。**--to の canonical 解決先が当該 scope root
  配下 (`.kio` 含む) の場合は KIO-E-CONFIG-USAGE-001 (exit 2) で拒否** — `--to .` による禁止の迂回を
  許さない。canonical 解決は §1.8 の canonical root_path 算出規則と同一 (realpath 含む) を --to と
  scope root の双方に適用する)
- **全出力 path について、退避 (`<basename>.kio-restore-bak`) / 隔離 (`<basename>.kio-restore-quarantine`)
  の同名残存を --force の有無・宛先の存否に関わらず mutation 前に検査し、残存 = 先行 restore の
  未完として拒否 + 回復案内する** (正本 §3.5 — --force 文脈に限定しない)
- 既存ファイル上書きは --force 必須 + 確認プロンプト
- --force 上書きは旧ファイルを同 directory の退避名 `<basename>.kio-restore-bak` へ no-replace で
  保全 (同名残存 = 先行未完として拒否 + 回復案内。退避名は stderr に表示・dev/inode を記録) して
  から publish し、rename 後再検査の purge / erase / journal 終端時のみ原状復帰する (対象 alive の
  無関係変化は publish 維持 — §3.5。成功時に退避を除去)
- publish (--force 含む)・隔離・復帰の rename は全て no-replace。巻き戻しの削除も退避の復帰・
  除去も、path 上の対照ではなく決定的隔離名 `<basename>.kio-restore-quarantine` への隔離 rename +
  rename した実体の dev/inode 検証で行う (隔離名は stderr に表示。同名残存 = 先行未完として拒否 +
  回復案内。隔離・退避はユーザー領域 — 04 §1.1 の temp 掃除の対象外、Kio は自動削除しない)
- 競合処置は段階別 (--force publish 競合 = 退避を復帰 / 隔離実体の不一致 = 元 path へ復帰を試行 /
  退避の不一致・復帰 rename 失敗 = 不触) — いずれも両所在を表示して
  KIO-E-COMMIT-RESTORE-CONFLICT-001 (retryable exit 3、context に conflict_kind・retry_disposition)
  で終端 (§3.5)
- 出力名・上書き対象名が `.kio-restore-bak` / `.kio-restore-quarantine` で終わる場合は展開前に
  明示拒否 (退避・隔離名前空間の予約 — 改名復元を案内)
- restore は raw object をそのまま展開 (再 Markdownize しない)
- shallow commit からの restore は KIO-E-COMMIT-SHALLOW-001
- purged 対象は KIO-E-PURGE-NOT-FOUND-001 / tombstone
- 展開は検証済み --to ディレクトリの dirfd 配下で no-follow (symlink を辿らない) に行い、
  private temp → atomic rename で publish する。**containment 判定と展開の同一実体束縛**: --to を
  O_DIRECTORY で open し、fstat (dev/inode) を canonical 解決先の lstat (**containment 判定時に
  取得した値** — 対照時の再取得は判定後の中間 component 差し替えで移動した実体を正当化するため
  用いない) と対照して同一実体を確認
  してから、以後の temp 作成・rename を全て同一 dirfd 配下に限定する (判定後の path 差し替えで
  別実体を指させない)。対照不一致は KIO-E-CONFIG-USAGE-001 (exit 2) で mutation 前に拒否する
  (--to 実体の検証失敗 = 不正オペランド)。絶対 path・「..」を含む復元エントリは拒否
  (既存 symlink 経由で復元先の外部を上書きさせない)
```

## 4.2 kio view (過去版閲覧)

```bash
kio view <evidence-at-commit-X>
kio view <path> --at <commit>
```

過去 commit 時点の Markdown を再生成せず、当該 commit の object をそのまま返す (re-Markdownize しない)。unit の完成状態・列挙は、当該 commit の tree entry `normalize.manifest_hash` が指す **manifest object** ([03-data-model.md §2.1](03-data-model.md)) で確定する — same-gen partial retry で作業コピー manifest.json が進んでいても、表示は commit 時点の manifest に従う。

**過去版に専用の経路は無い (2026-08-11 確定)。**全文 view のパスは
`(raw_hash, tool_profile_hash, gen)` だけで決まり ([03-data-model.md §2.1](03-data-model.md))、
**commit は入らない**。過去版は内容が違うので別の `raw_hash` を持ち、したがって
別の view パスを持つ — それだけである。`--at` の役割は「**どの `raw_hash` のことか**」を
指す解決手段にすぎず、解決した後の出力は §1.7.2 と同一である:

| 指定 | 解決 | 出力 |
|---|---|---|
| `kio view <pointer>` | pointer が `raw_hash` を持つ | 全文 view のパス + view-local span |
| `kio view <path> --at <commit>` | path@commit → manifest object → `raw_hash` | 同上 |

したがって「過去版閲覧」という別モードを実装しない。`--at` は前段の解決に閉じる。

# 5. プロセスモデル (常駐なし)

Kio は **常駐 daemon を持たない**。すべての処理は CLI コマンドのプロセス内で完結する。

- interval 発火 (定期 auto snapshot, Phase 4) は OS スケジューラ (launchd / systemd user timer / Task Scheduler) から `kio snapshot auto` を起動する委譲方式とする (§8.2)
- idle 検出 (`gc.mode="on_idle"`) も OS scheduler が起動する `kio snapshot auto` の都度に判定し、Kio 自身は常駐しない (§2.3, §8.2)
- 同一 `.kio` に対する多重起動は `.kio/.lock` で防止する (§6)
- **ローカルモデルサーバ (`execution_mode = "offline_api"`) の重み常駐は、サーバ側の責務であって Kio の責務ではない** (2026-07-26)。Kio はプロセスを起動も管理も常駐もせず、[07-adapter-spec.md §3](07-adapter-spec.md) が定める loopback url を呼ぶだけである — この点で online_api Adapter の呼出と何ら変わらない。モデルの遅延ロード・idle TTL による重み解放は当該サーバの設定であり、Kio の「常駐なし」原則と矛盾しない。サーバが起動していない場合の扱いは §1 の既存縮退に従う (embedding なら text fallback)

# 6. 並行性 / Locking

```text
.kio/.lock                     プロセスレベル排他 (書き込み系コマンド全般、下記)
.kio/index/sqlite.db (WAL)     reader と writer の整合性
```

`.kio/.lock` を取得するコマンド (書き込み系):

```text
kio index / kio snapshot create / kio snapshot auto / kio tag (refs/tags-v1 更新) / kio gc / kio purge /
kio repair rebuild-db / kio repair verify-objects /
kio batch resume / kio batch retry / kio batch abandon / kio reindex /
kio adapter revoke
```

承認系の scope.json 更新 — 承認操作 (対話 / `--approve` の行 publish)・approval self-heal・
`kio adapter revoke` — は、いずれも上記 lock 下の locked mutation として直列化する
(並行する approve × revoke の lost update を作らない — [07-adapter-spec.md §3](07-adapter-spec.md))。
承認 publish 直前の CAS 再検証の不一致は `KIO-E-ADAPTER-APPROVAL-CONFLICT-001` (exit 5 —
並行 revoke による pending 除去、再承認が必要。[07-adapter-spec.md §3](07-adapter-spec.md)) で終端する。

batch 系と reindex は外部副作用 (upload / job 作成) と batch_requests の状態遷移を伴うため lock 必須
([04-pipeline.md §5.8](04-pipeline.md) — 並行 resume が同一行へ別 intent_token を書くと先行 job が
無記録 in-flight になる)。

規約:

- 読み取り系 (search / log / view / open / inspect / evidence verify / restore / status / diff) は `.kio/.lock` を取得しない。`kio index` と `kio search` の同時実行は許容する。検索は `.kio/index/sqlite.db` の WAL snapshot を読まず、公開済み `aggregator.sqlite` の projection だけを読む。`index_status.budget_paused` の月次 cap 観測は、1 search invocation につき device-global cost-ledger の owned read-only snapshot を command preflight で1個だけ作る。取得は既存の入力・mode 判定後、fresh vector / hybrid page 1 の writable ledger open・claim / reservation・query embedding 送信より前に完了する。同じ snapshot と取得時の UTC 集計月を response まで保持し、取得後の別 writer や同一 invocation による charge はその response の月次 cap 観測へ混ぜない。source main / `-wal` / `-shm` を NOFOLLOW・regular・single-link・identity・size・SHA で前後観測し、owner-private temp へ main と存在する WAL だけを複写して `READ_ONLY` + `query_only` で読む (SHM は複写しない)。全3 leaf absent のみ no-ledger / spent=0 とし、partial leaf・unsafe link / replacement・stable-copy integrity failure は `KIO-E-LEDGER-SNAPSHOT-UNSAFE-001` / exit 4、presence/hash drift・busy・retry exhaustion・temp/copy/open/query の不確定性は `KIO-E-LEDGER-SNAPSHOT-001` / exit 3 で stdout を返さず fail-closed する。これは開始と終了の一致を要求する stable-or-fail 観測であり、cross-file formal atomic snapshot の主張ではない。例外的に `kio search` は final consent check を通過した vector|hybrid の fresh page 1 に限り cost-ledger.sqlite の device 行 (`scope_id='device'`) への相 1 / stale 回収・剪定の書込を行う。claim 開始後の adapter failure で最終結果が text fallback になってもこの例外は維持される。一方、offline / profile incompatibility / final consent rejection による pre-attempt auto→text はこの書込を行わない。これも `.kio/.lock` の対象外である — device 行はどの scope にも属さず、直列化は cost-ledger 側の `BEGIN IMMEDIATE` Tx が担う ([04-pipeline.md §5.4](04-pipeline.md))
- `.kio/.lock` を取得できない場合、書き込み系コマンドは**待機せず即座に失敗する**: error code `KIO-E-STORE-LOCKED-001`、exit code 3 (retryable、[06-cli-spec.md §7](06-cli-spec.md))。lock ファイルには保持プロセスの pid と取得時刻を記録し、保持プロセスが存在しない stale lock は次の取得試行時に回収してよい。Unixでは acquire/reclaim/release 全体を crash-release される directory `flock` でも直列化し、release はcheck-then-unlinkでなくdead canonical sentinelとのatomic exchangeを使う。このため非保持時にもdead sentinel leafが残り得るが、次writerが同じgate下で回収し、単なる`.lock`存在だけをlive判定に使わない。
- refs (refs/heads/main, refs/tags-v1/*) の更新は `.kio/.lock` 保持下で、temp file 書き込み + atomic rename により行う (部分書き込みを外部に見せない)
- `kio repair verify-objects` の raw object 復旧と repaired commit publication も、同じ lock の下で private temp + hash 再検証 + atomic publish を使う
- `kio repair rebuild-db` 実行中の `kio search` は旧 `.kio/index/sqlite.db` を読まない。scope の replica header が `Rebuilding` または完全 projection を欠く間は `KIO-E-INDEX-REBUILDING-001` で fail-closed とし、writer が新しい projection を publish してから検索へ再参加させる。再構築本体は引き続き atomic rename (`sqlite.db.tmp → sqlite.db`) で切り替える
- `kio repair replica` / `kio repair all` は registry の indexed scope を決定的順序で走査し、各 scope の `.kio/.lock` を 1 個ずつ取得する。`replica` は source を変更せず完全射影だけ、`all` は objects 検証・source SQLite rebuild・完全射影を行う。active purge journal の scope は修復対象として開かず partial failure に残し、replica の旧本文を Ready として再公開しない
- scope-registry.sqlite / cost-ledger.sqlite (~/.local/share/kio/) は WAL モード + busy_timeout (デフォルト 5000ms) で複数プロセスの同時書き込みを直列化する。registry は cache であり ([03-data-model.md §4](03-data-model.md))、破損時は各 `.kio` の rescan で再構築する (**再構築の入力はユーザーが知る探索 root** — registry 喪失後は `.kio` の所在一覧も失われるため、各 root での `kio index` 再実行が再登録を兼ねる。Kio が自力で全ディスクを走査することはしない)。cost-ledger.sqlite は**再構築不可の運用台帳** ([03-data-model.md §4.1](03-data-model.md) / [04-pipeline.md §5.4](04-pipeline.md))
- purge の log scrub と通常 append/rotation は、device logs では `${XDG_DATA_HOME:-$HOME/.local/share}/kio/logs/scrub.lock`、scope access logs では `.kio/logs/access.scrub.lock` を共有する。複合 lock 順序は scope store → cost-ledger.sqlite (Tx) → device observability → scope access とし、逆順取得を禁止する。**scope 由来 log の append 順序**: 読取系が対象の path / query / raw_hash を含む行を append する場合、当該 append は scrub lock を保持したまま、3 点検査 (§6 — journal 不在 + epoch 不変 + lifecycle counter 不変) の**最終検査と同一 critical section** で行う — scrub 完了後の再 append で purge の削除 postcondition を破らない。最終検査で拒否した場合の記録には対象 path / query / raw_hash を含めない

# 7. 観測 (Observability)

```
~/.local/share/kio/logs/
  events.jsonl       重要イベント (commit, gc, purge, schema migration)
  metrics.jsonl      数値メトリクス (デフォルト 1h 間隔の集計に加え、下記の per-search 記録)
  errors.jsonl       error_code 付きの全エラー
.kio/logs/
  access.jsonl       検索アクセスログ (redact_logs はデフォルト true、10-operations.md §11.6)
```

**検索 latency の per-search 記録** (2026-07-03 追記、step3a §C の解消。北極星 §4.1 の p50/p95/p99 計測の一次データ): `kio search` は 1 回の実行ごとに metrics.jsonl へ 1 行を追記する。行はログ共通 envelope (必須 `ts, level, code, component, message, context`) に従い、metric 固有フィールドを加える — `{ "ts": <UTC>, "level": "info", "code": "KIO-M-SEARCH-001", "component": "search", "message": "search completed", "metric": "search.latency_ms", "value": <実測 ms>, "context": { "mode": <実効 mode>, "scope_count": <検索した scope 数>, "result_count": <返却件数> } }`。redact_logs 既定 (クエリ本文・path は記録しない) に従う。1h 間隔の集計メトリクスはこの一次データから導出してよい。非エラー行の `code` は `KIO-M-<DOMAIN>-<NNN>` (metric) / `KIO-EV-<DOMAIN>-<NNN>` (event) の名前空間を使う — 形式は [06-cli-spec.md §8](06-cli-spec.md) の error_code と同じ規約 (`KIO-E-` は error 専用)。

各行 JSON 必須フィールド: `ts, level, code, component, message, context`。詳細は [10-operations.md §11.6](10-operations.md)。

# 8. Auto Commit

## 8.1 MVP (Phase 1-3) の snapshot 契機

MVP での snapshot 生成契機は次の 3 つのみ (常駐プロセスは持たない、§5):

1. 明示的 `kio snapshot create` (commit_type=manual)
2. `kio index` の成功完了時に同一プロセス内で auto snapshot を作る (commit_type=auto)。ただし tree_hash が現在の HEAD の tree と一致する場合は commit を作らない (no-op、[03-data-model.md §8.2](03-data-model.md))
3. `kio batch resume` / `kio batch retry` / `kio reindex --regenerate` が online 成果 (normalized / chunk) を finalize した成功完了時も同じ publication protocol を完了する ([04-pipeline.md §5.4](04-pipeline.md))。chunk の検索 authority は tree の集合値ではなく、introduction commit と config を明記した tagged publication event で確立する (§1.6)

**no-op 規則の例外**: resurrection finalize (erase / purge 済み raw の再 ingest) は、同一 bytes の再現でも、retire event と新しい introduction を確定する commit が必要なら publication を行う。no-op 判定は tree_hash に加えて commit の `tool_lock_hash` も比較する。

**snapshot finalize の耐久順序**: (1) chunks.jsonl へ creation / publication event 行を append + fsync → (2) SQLite 反映 → (3) commit / ref publish。(1) と (3) の間の crash で dangling event 行が残った場合、rebuild はそれを無視し ([04-pipeline.md §5.7](04-pipeline.md) と同一条件 — 生存する creation 行 / chunk object を持たない、**または** introduction commit の **object が store に存在しない**行。commit object が存在するが ref 不達の行 (orphan / disconnected — `--at` の正当対象) は無視しない)、次回 finalize が同内容を冪等に再 append する。chunks.jsonl 末尾の不完全行 (torn tail) は切り詰めて無視する (書込は [04-pipeline.md §1.1](04-pipeline.md) の fsync 規律)

## 8.2 定期 Auto Snapshot と on_idle GC (Phase 4 milestones 4–5)

```text
- ユーザー操作なし時に一定間隔で auto snapshot を作る (commit_type=auto)
- 実行主体は常駐 daemon ではなく、OS スケジューラ (launchd / systemd user timer /
  Task Scheduler) から起動される CLI とする (§5)。多重起動・kio index との競合は
  .kio/.lock で排他する (§6)
- snapshot 対象は indexed scope の現在 working tree
- auto commit は tiered retention で減衰する (§2.4)
- manual commit は auto を吸収しない (auto は tiered retention 満了で shallow 化され tree を失うが — ref tip が指すものは除外 (§2.4) — commit object は履歴 DAG の中間点として残る。§2.2)
- tree_hash 不変なら no-op (§8.1 と同じ)
```

canonical entrypoint は `kio snapshot auto` だけである。Kio 自身はtimer/daemonを作らず、
実機schedulerをinstallするサブコマンドも持たない。OS schedulerはindexed scopeをworking
directoryとして、任意の短い周期でこのコマンドを起動してよい。実行ごとに注入済みcurrent
UTC seconds、strict config、durable checkpoint、HEADとdescriptor-bound working tree scanから
次を決定する。

```text
first_run                     stateが無ければeligible
interval_elapsed              now >= last_successful_eligible_attempt_at + interval_seconds
change_threshold              raw pathのadd/edit/delete合計 >= on_change_threshold
interval_and_change_threshold 上2条件が同時に成立
not_eligible                  上記のいずれも不成立
```

`first_run`、interval、thresholdのいずれかを満たしたときだけsnapshotを試みる。eligibleな
attemptはworking bytesとimmutable tree/commitを準備・再検証した後、HEAD/ref/manifestより先に
`.kio/snapshot-auto.json` (version 2, JCS canonical JSON+LF) の state をconditional publishする。
required field は `version=2`、`last_successful_eligible_attempt_at`、`working_set_digest`、
`idle_observed_since` の4個だけである。v1 は migration / dual read をせず reject する。state CASが競合した場合はrefを
進めず、同時writerのstateを上書きしない。eligibleでもtreeと`tool_lock_hash`の両方がHEADと
同じならcommitを作らないが、no-op自体は成功したattemptなのでcheckpointを進める。checkpoint
publish後のscope/policy再検証またはref publishがfail-closedした場合、既に耐久化したcheckpointは
rollbackしない。この保守的cooldownは同じ失敗をfirst-runとして反復することを防ぎ、準備済みだが
ref不達のCAS objectはauthorityにならない。lock競合、checkpoint以前のauthority差替えではstateを
更新しない。clockがcheckpointより過去へ戻った場合は`KIO-E-SNAPSHOT-CLOCK-001` / exit 3で
fail-closedする。checkpointはprivate tempのfile fsync後にatomic publishし、公開前crashで残った厳格な
`.snapshot-auto-state-<pid>-<nanos>` residueは次回writerが最大64件まで検証・回収する。
malformedな予約namespace、上限超過、非canonical recordは削除せずfail-closedする。

change countはcurrent HEADと現在のdirect-scope regular filesのraw hashを比較する。`working_set_digest`
は同じ path→raw hash の JCS digestであり、同じ filtered input（generated parent policy、`[scope].ignore`、
`.kioignore`、Tier A）だけを含む。mtime は digest に含めない。既存の
generated parent policy、`[scope].ignore`、`.kioignore`、Tier A規則をそのまま適用し、directory、
symlink/reparse、hardlink、special/non-UTF-8 leafをworking fileへ暗黙変換しない。unchanged rawは
HEAD treeに固定されたimmutable normalize referenceだけを持ち越す。changed/new rawへ過去の
normalize referenceやmutable cacheを流用しない。

`on_idle` の activation は `gc.mode="on_idle"`、enabled な `[snapshot.auto]`、indexed scope の全てを
要する。config欠落/disabled/not-indexed の skip は read-only で、GC を発火・resume しない。
first observation は baseline を記録するだけで GC を実行せず、digest が変われば
`idle_observed_since` を now へ reset して GC を実行しない。同一 digest が threshold 以上継続した時だけ
eligible となる。scheduled snapshot 成功後に `after_index` を新規発火しない。`on_idle` GC は writer
publication の後、lock release の後だけに実行し、timeout は exit 3、integrity failure は exit 4、
scheduled mutation 非対応 platform は state / lock / HEAD より前に unsupported で fail-closed する。snapshot config、GC config、
index metadata、state、scope/`.kio` identity、scanはwriter lock前後とpublication直前に再検証し、
差替えをskipへ縮退しない。完全なignore authority（validated config、generated parent policy、
root `.kioignore`）はretained handleで固定し、最初のraw CAS publish直前、checkpoint/ref境界、
publication後にexact observationを再検証する。publication自体はretained scope/`.kio` capabilityへ
固定し、public path差替え後のstoreへreentrant lockを誤適用しない。

`.kio/config.toml`:

```toml
[snapshot.auto]                 # Phase 4 (定期 auto snapshot)
enabled = true
interval_seconds = 1800     # 30 分ごと
on_change_threshold = 50    # 50 ファイル以上の変更で即時 snapshot
```

section欠落はdisabledである。sectionがある場合は上記3 fieldをすべて必須とし、alias、旧名、
暗黙default、unknown fieldを受理しない。範囲は`interval_seconds=1..31536000`、
`on_change_threshold=1..1000000`である。

OS scheduler設定例（説明用。Kioはこれらをinstallしない）:

```text
launchd:        ProgramArguments = ["/absolute/path/kio", "snapshot", "auto", "--json"]
                WorkingDirectory = "/absolute/indexed/scope"
systemd --user: WorkingDirectory=/absolute/indexed/scope
                ExecStart=/absolute/path/kio snapshot auto --json
```

現行のscheduled mutationはdescriptor-relative writer lockとatomic state exchangeを実装済みの
macOS / Linuxだけで有効である。Windowsおよびその他のplatformでは、eligibleな実行を
`.kio/.lock`・HEAD・scheduler stateへ触れる前に
`KIO-E-SNAPSHOT-PLATFORM-UNSUPPORTED-001`でfail-closedする。

JSONは`operation,status,reason,publication_status,snapshot_status,eligibility_reason,eligible,change_count,next_eligible_at,
commit_hash,tree_hash,stats,working_set_digest,idle_observed_since,idle_observed_seconds,
idle_threshold_seconds,idle_eligible,recovered_gc,recovery_pending,gc`を固定fieldとして返す。
on_idle の human / JSON output は `baseline_recorded`、`not_idle`、`completed`、`noop`、`deferred` を
同じ field set で返す。GC は `gc` object に status / reason / mode を返す。human outputも同じ分類と
next eligible時刻を決定的に表示する。

state が unsupported / malformed で clean recreation が必要な場合は、Kio writer を停止し、
`.kio/gc/in_progress` が無く `.kio/.lock` に live operation が無いことを確認してから、正確に
`.kio/snapshot-auto.json` の leaf だけを除去する。広い `rm` は使わない。次の `kio snapshot auto` が
baseline state を v2 として再作成する。
