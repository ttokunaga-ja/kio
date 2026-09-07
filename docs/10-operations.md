# 10 Operations (横断規約と運用)

この文書は、実装・UI・運用へ落とすときに問題になりやすい点を補足する。

> **NOTE (2026-05 改訂)**: ポジショニング・ターゲットユーザー・MVP 境界の考え方は **正本を [01-positioning.md](01-positioning.md) に移した** (機能 × Step 割当・実装時期の正本は [09-mvp-scope.md §3.1](09-mvp-scope.md) — README §1)。本書はその下位の運用ルールを扱う。競合分析は [01-positioning.md §4](01-positioning.md) を参照。

MVP は **「Evidence-grounded local knowledge archive」としての最小完全系** として扱う。「全部入りの Git for knowledge」を目指さない。詳細は [01-positioning.md §5](01-positioning.md)。

---

# 1. 初回スキャン前の承認

Kio はデフォルトで全 indexed scope を検索対象にし、全ファイルを管理対象にする。ただし、初回スキャンでは、対象範囲 preview、除外提案、明示承認を必須にする。

目的はデフォルト全管理を弱めることではない。Kio が単なる検索インデックスではなく、原本を content-addressed object として保存する知識アーカイブであることを、ユーザーが理解したうえで開始するためである。

必須フロー:

```text
kio init
  ↓
候補 scope を探索
  ↓
対象フォルダ / 推定ファイル数 / 推定容量 / 大容量ファイル / 除外候補を preview
  ↓
.kioignore / 設定を調整
  ↓
再 preview
  ↓
明示承認
  ↓
raw object 保存、Markdownize、Embedding、index 更新を開始
```

preview では、少なくとも次を表示する。

```text
root path
included scopes
excluded scopes
estimated file count
estimated total bytes
large files
hidden directories
build/cache/vendor candidates
network transmission policy
adapter execution mode
estimated markdownize cost (USD)
estimated embedding cost (USD)
estimated completion under current budget cap
```

コスト概算は、現行 `tools.toml` の `[pricing]` 単価表 ([07-adapter-spec.md §4](07-adapter-spec.md) — 単価の正本は tools.toml であり tool-lock ではない) × 推定ページ数 / トークン数から算出する **桁の目安** であり、保証ではない。概算合計が当月の effective budget cap ([04-pipeline.md §5.4](04-pipeline.md)) を超える場合、preview は承認前に警告し、cap 内での推定完了時期 (月数) とあわせて次の選択肢を提示する。

```text
Estimated AI enrichment cost: ~$210 (markdownize ~$180, embedding ~$30)
Current budget cap: $50/month → estimated completion: 5 months
Options:
  [1] ベースライン index のみで開始 (コスト $0。AI 強化は後から)
  [2] 除外 (.kioignore) を調整して再 preview
  [3] budget cap を変更
  [4] このまま続行 (cap 到達時に AI 強化タスクは paused)
```

ベースライン index ([07-adapter-spec.md §2.1](07-adapter-spec.md)) は、**明示承認後の実行において** online 強化タスクの成否・budget 状態に依らず先に完了するため、承認初日の検索は成立する。**[2] / [3] の再調整中は raw object 保存を含む一切の取り込みを開始しない** — 上記フローの「明示承認 → 開始」が正 (承認前に archive しない)。

除外候補は提案であり、ユーザーの承認なしに自動除外しない。唯一の例外は secrets 系パターン
(§1.1 Tier A) で、これは built-in デフォルト除外として最初から「除外済み」状態で preview に
表示され、取り込むにはユーザーの明示的な解除操作 (対話承認時の個別選択、または .kioignore の
negation 記述) が必要である。`--yes` はこの解除を行えない ([06-cli-spec.md §2](06-cli-spec.md))。

```text
Suggested exclusions:
  node_modules/     build/cache candidate
  target/           build output candidate
  .git/             VCS internal metadata
  *.tmp             temporary file
  *.cache           cache file
  video.mp4         large file: 8.2GB
```

secrets 系はデフォルト除外・警告として別枠で表示する。

```text
Excluded by default (secrets, Tier A):
  .env              environment file
  .ssh/             SSH keys directory
  cert.pem          private key / certificate

Sensitive candidates (Tier B, 取り込み予定・要確認):
  db_credentials.yaml   filename matches *credentials*
  api_tokens.md         filename matches *token*
```

非対話環境では、承認済み scope または `--yes` / `--approve` のような明示オプションがない限り、`kio index` は失敗させる。

承認記録には、少なくとも次を残す (**保存先 = `.kio/scope.json` の `scan_approval` key** — schema 検証
対象 §11.3。adapter 単位の network opt-in 承認 `approvals[]` ([07-adapter-spec.md §3](07-adapter-spec.md))
とは別 key)。

```text
scope_id
root_path
approved_at
actor
approval_method        # interactive | approve | yes
kio_version
effective_ignore_hash
estimated_file_count
estimated_total_bytes
estimated_markdownize_usd
estimated_embedding_usd
```

承認後の index は二段で進む ([04-pipeline.md §5](04-pipeline.md)): ベースライン index が先に完了し、AI 強化 (Markdownize / Embedding) は budget guardrail の管理下で後段として進む。AI 強化が未完了・paused の間、その状態を隠してはならない。

- `kio status` は AI 強化の進捗 (done / pending / paused 件数) と paused の理由 (budget / auth / tier_b_approval) を表示する (rate limit は paused ではなく pending + next_retry_at として表示 — [04-pipeline.md §5.2](04-pipeline.md))
- 照合が恒久不能な in-flight Batch job (資格情報喪失等) は **stalled** として表示し続ける。脱出路は
  `kio batch abandon` のみ (自動では何も変更しない — [04-pipeline.md §5.8](04-pipeline.md))
- 検索レスポンスは index が部分的なとき `index_status` を返す ([05-runtime.md §1.7](05-runtime.md))

## 1.1 Secrets デフォルト除外 (built-in ignore template)

Kio は secrets 系ファイルの取り込み・オンライン送信事故を防ぐため、built-in の除外テンプレート
を同梱する。パターンは 2 段階に分ける。

**Tier A (デフォルト除外)**: 拡張子・ファイル名から secrets とほぼ確実に判定できるもの。
system directory (§4 の走査境界既定) も Tier A 相当の built-in 除外に含め、**OS 別の対象パターンは
built-in template に列挙し、その template の版を `effective_ignore_hash` の入力に含める** (パターン
更新が承認記録の同一性判定に反映されるように)。
初回 preview で「除外済み」として表示され、取り込むには明示解除が必要。

```text
.env
.env.*
*.pem
*.key
*.p12
*.pfx
id_rsa*
id_ecdsa*
id_ed25519*
*.keystore
.ssh/
.gnupg/
.aws/
.kube/config
.docker/config.json
.netrc
.npmrc
.pypirc
*.tfstate
*.tfstate.*
```

**Tier B (警告のみ)**: 名前ベースで機微の可能性があるが誤検出も多いもの。取り込み対象に
含めるが、初回 preview の「機微ファイル候補」欄に列挙してユーザー確認を促す。

```text
*credentials*
*secret*
*token*
*apikey*
*password*
```

規約:

```text
1. テンプレートは Kio 本体に同梱し、バージョンを effective_ignore_hash の入力に含める
2. Tier A の解除は、対話承認時の個別選択 または .kioignore の negation (!pattern) のみ
3. --yes は Tier A の解除・Tier B 警告のスキップを行えない (06-cli-spec.md §2)
4. テンプレートの追加・変更は本節の更新を伴う (破壊的変更扱い)
```

**承認後に追加されたファイルの扱い**: scope 承認は初回一回だが、承認後にフォルダへ追加された
ファイルが secrets パターンに一致する場合は自動処理を保留する。

```text
Tier A 一致の新規ファイル:
  取り込み自体を保留 (quarantine)。CAS 保存・snapshot への取り込みを行わない。
  kio status に「取り込み保留 (secrets 候補)」として表示し、
  取り込みには対話確認 または .kioignore の明示編集を要する。
  対話確認による取り込みは当該 raw_hash の取り込みとして完結する (再確認は内容変更 =
  新 raw_hash の時のみ — 変更版を再 quarantine するのは意図的な保守既定)。

Tier B 一致の新規ファイル:
  ローカル取り込み (CAS 保存・ローカル index) は行うが、online_api Adapter への
  送信 task は **paused (hold_reason=tier_b_approval — [04-pipeline.md §5.2](04-pipeline.md))**
  として保留し、kio status に表示する。
  対話確認 (kio index の実行時プロンプト) で一括承認できる (承認 = paused 解除)。

非一致の新規ファイル:
  従来どおり自動取り込み (デフォルト全管理を維持)。
```

---

# 2. 容量より利便性を優先する

Kio は、容量効率よりも知識を失わないこと、あとから検索・履歴探索・復元できることを優先する。

したがって、全ファイル管理をデフォルトとする方針は維持する。動画・巨大PDF・画像・Officeファイルも、ユーザーが明示的に ignore しない限り管理対象に含める。例外は secrets 系の built-in デフォルト除外 (§1.1 — 不可逆な漏洩リスク) と、§4 の走査境界既定 (system directory / VCS repo root / placeholder 等 — 安全側の既定であり容量目的ではない) のみ。

ただし、プロダクトはこの事実を隠してはならない。

```text
Kio は検索インデックスだけでなく、原本ファイルを content-addressed archive に保存します。
各 `.kio` が管理するのはその `.kio` が置かれたフォルダ直下のファイルのみです。
サブフォルダのファイルは (そこに `.kio` があるか否かに関わらず) 親 `.kio` は取り込みません。
対象ファイルを含むサブフォルダには子 `.kio` が作られ、独立したスコープとして管理されます
(§4 の既定により VCS repo root 配下には作られません)。
同じ `.kio` 内では同じ内容を重複保存しません。
別フォルダの別 `.kio` に同じ内容のファイルが存在するのは、ユーザーが意図的に複数フォルダへ
同じファイルを配置した場合に限られ、その場合はフォルダ単位の独立性を優先して重複保存します。
```

必要な表示:

```text
推定追加容量
`.kio` 内 dedup 後の保存見込み
別 `.kio` 間で重複する可能性のある容量 (ユーザーが複数フォルダへ同じファイルを配置している場合のみ発生)
大容量ファイル一覧
現在の空き容量
ディスク枯渇リスク
除外候補
```

ディスク枯渇が予測される場合、Kio は勝手に対象範囲を狭めない。続行、除外、延期、中断をユーザーに選ばせる。

---

# 3. Scope Registry (= cache only, NOT truth)

Kio は **二層構造** をとる。データ・所有権・権限の **正本は各フォルダ直下の `.kio`** に閉じる。device-local な scope_registry と global aggregator は **検索キャッシュに過ぎない**。両者を混同しない。

```
truth = folder-local .kio
  raw object / normalized / chunks / commits / refs
  purge の単位

cache = scope_registry
  検索の探索対象一覧、stale 検出
cache = aggregator
  全 scope の chunk (live + 過去) の read replica (横断検索の採点・候補選択)
  権限状態の横断投影 (投影のみ — 送信 gate の判定には用いない)
```

実装では、device-local な scope registry を明確に持つ。

保存先:

```text
~/.local/share/kio/scope-registry.sqlite
```

schema (本節が正本。2026-07-14 実装準拠で確定):

```sql
CREATE TABLE scopes (
  scope_id TEXT NOT NULL,
  kio_path TEXT NOT NULL,
  root_path TEXT NOT NULL,
  participates_in_global_search INTEGER NOT NULL DEFAULT 1
      CHECK (typeof(participates_in_global_search) = 'integer'
             AND participates_in_global_search IN (0, 1)),
  indexed INTEGER NOT NULL DEFAULT 0    -- scope 索引を構築済み（aggregator の複製対象として列挙可能）
      CHECK (typeof(indexed) = 'integer' AND indexed IN (0, 1)),
  last_seen_at TEXT NOT NULL,
  PRIMARY KEY (scope_id, kio_path)
);
```

運用規約:

- WAL モード + busy_timeout 5000ms で複数プロセスの書き込みを直列化する
  ([05-runtime.md](05-runtime.md) 同時実行規約)
- EvidenceReadOnly の registry 解決は source registry を SQLite で直接開かない。main / `-wal` /
  `-shm` を NOFOLLOW・regular・identity・size・SHA で前後観測し、private TempDir へ main と
  存在する WAL だけを copy して query_only reader を開く。3 leaf の presence を含む比較が崩れた
  場合は stable-or-fail（formal atomic snapshot の主張ではない）で停止する。main / `-wal` / `-shm` の
  **全 3 leaf が absent** の場合だけが no-create cache miss であり、main absent + sidecar present は
  unsafe integrity。unsafe leaf・pre/open/post identity mismatch、または source 再観測が一致した
  owned copy の SQLite integrity failure は `KIO-E-REGISTRY-SNAPSHOT-UNSAFE-001` / exit 4、presence/hash
  drift・busy・retry exhaustion・temp I/O・integrity を確定できない private snapshot read-open-query
  failure は `KIO-E-REGISTRY-SNAPSHOT-001` / exit 3。これらを hint / CWD fallback や scope status に
  落とさない。
- upsert は `(scope_id, kio_path)` を key に行い、`indexed` は単調 (MAX) にのみ更新する。`root_path` /
  `kio_path` は canonical 形で保存する (規則の正本 = [05-runtime.md §1.8](05-runtime.md): 絶対化 →
  lexical 解決 → 末尾 separator 除去 → realpath、比較は byte 単位)
- **stale 登録の退役**: `.kio` を削除して同じ path で `init` し直すと新しい `scope_id` が採番される
  (scope_id は init 時採番の ULID、[03-data-model.md §2](03-data-model.md))。upsert の直前に、同一
  `kio_path` で `scope_id` が異なる行を削除する。**逆方向 (scope の移動) も同様に退役する**: 同一
  `scope_id` を新しい `kio_path` で観測 (再発見) したら、同一 scope_id の旧 path 行を削除する —
  **ただし旧 path がなお到達可能 (存在し有効な `.kio`) な場合は move と認定せず、削除しない**。
  同一 scope_id の複数 live path は clone 併存であり、**fail-closed で扱う**: global search は当該
  scope_id を skip して `excluded_scopes` に `KIO-E-REGISTRY-DUP-001` の理由付きで記録し、pointer 解決は
  候補一覧 error とする ([08-evidence-pointer-spec.md §3.1](08-evidence-pointer-spec.md) — purge 状態の
  異なる clone へ黙って解決しない)。どちらを残すかはユーザーの dedupe に委ねる —
  stale 行 (到達不能な旧 path) を放置すると、default 横断検索が毎回 skip して恒常的に partial (exit 3)
  になり、その行経由の Evidence Pointer は `KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001` になる — これが退役の
  理由。**再 init・再発見のどちらも起こらない恒久消滅** (ドライブ撤去・`.kio` ごとの削除等) の
  stale 行は、`kio repair registry-prune` (確認プロンプト付き — 到達不能行を列挙し、live clone
  検査 (上記) に該当しない行のみ削除。[06-cli-spec.md §1](06-cli-spec.md)) で退役できる —
  registry は検索キャッシュであり、削除しても truth は失われない (scope が再出現すれば再発見で
  再登録される)。live 重複はこれとは別で、上記のとおり fail-closed (search skip + 解決 error) で扱い、
  黙って二重に返すことはない。**live 重複が解消するまでは、当該 scope_id での書き込み系コマンドと
  online タスク起動 (相 1) も `KIO-E-REGISTRY-DUP-001` で fail-closed とする** — device-global
  `batch_requests` の行 (PK に scope_id) を複数 clone が共有し、回復・終端・課金の帰属が混線するため
  ([04-pipeline.md §5.8](04-pipeline.md))。dedupe 後に再開する。
  **複合状態の優先順位 (全コマンド共通の preflight 順序)**: **(0) `kio_format_version` 互換判定
  (§11.5 — current `KIO_FORMAT_VERSION` と完全一致しない store は、全 command で KIO-E-STORE-VERSION-001 / exit 8。
  §11.3 の「schema validation より先」と同じ原則で、他のすべての検査に先行する)** →
  (1) purge journal / epoch 検査
  ([05-runtime.md §3.5](05-runtime.md)) → (2) registry live 重複 (KIO-E-REGISTRY-DUP-001) →
  (3) index 可用性 (KIO-E-INDEX-REBUILDING-001) → (4) command 固有の検査。**(3) は復旧・初期化
  コマンド自身 (`kio repair rebuild-db`・index 未作成時の初回 `kio index`) には適用しない** —
  復旧経路を preflight でブロックすると index 喪失後に恒久停止する。`kio status` も拒否対象外
  (journal active 等を拒否せず状態として表示 — [05-runtime.md §3.5](05-runtime.md))。同時成立時は先順の
  error を返す。multi-scope の `excluded_scopes.reason` は command-wide の format-version gate を
  通過した後の (1)〜(4) だけを同順で決定し、version 不一致を scope 除外へ変換しない (実装順に
  依存させない — purge 再開・dedupe・rebuild のどれを先に行うべきかを automation が一意に
  判断できる)。読取系は
  この順序を**冒頭 1 回**適用し、その時点の registry / index 状態を線形化点とする — 返却直前の
  再検査は**冒頭で保存した開始値との不変比較**を固定順で行う: (1) purge journal 不在 →
  (2) purge epoch = 開始値 → (3) lifecycle counter = 開始値 (**counter が最終の線形化点**)。
  いずれかの不一致で結果を破棄し retryable (exit 3) — 比較対象は常に開始値であり、最新
  last_lifecycle_epoch との再照合ではない ([05-runtime.md §6](05-runtime.md) — 不可逆副作用と
  旧 cursor 受理の防止が目的)。検査後の DUP / REBUILDING の状態変化は次回実行で拾う
  (fail-closed の再適用はしない)。
  registry は cache なので削除は解決系には安全 (live scope は自分を再登録する)。**ただし live 重複の
  検出 (DUP gate) も registry に依存するため、削除直後は gate が一時的に盲目になる** — 各 clone の
  次回使用で再登録され次第回復する既知の窓 (削除を dedupe の手段にしない)
- device data dir (`~/.local/share/kio/`) は owner-only (0700) に制限する (best-effort。非 unix は
  no-op。registry / cost-ledger / logs は利用パターンとスコープ地図を含むため)


### 不変条件 (cache vs truth)

```text
1. scope_registry / aggregator のみを更新して `.kio` の状態が変わる実装は禁止。**反映順序は「各 scope の索引 → aggregator」** — 逆順は検索に出るのに開けない結果を作る (2026-08-11、05-runtime.md §1.8)。
2. scope_registry / aggregator 喪失は再構築可能 (各 `.kio` を rescan)。**「欠損中も動作」は要求しない** — 正しい挙動は検索を fail-closed とし、次の writer / repair が完全射影を再公開することである (03-data-model.md §4 不変条件 2)。読取り時の lazy refresh は行わない。
3. `.kio` 喪失は復旧不能 (registry・aggregator には正本データがない)。
4. 検索結果メタには「正本の `.kio` パス」を必ず含める。
5. raw object の所有権・dedup は scope_registry でグローバル化しない。
   各 `.kio/objects` 内に閉じる (横断 dedup を諦めた帰結。03-data-model.md §3)。
6. aggregator は安全性判定の最終権限を持たない (05-runtime.md §1.8 手順 3)。
7. aggregator は liveness 判定を再実装しない (解決済みの答えだけを複製する)。**検索が読む索引は aggregator ただ 1 つ** — scope 数によらず各フォルダの `.kio/index/sqlite.db` を `kio search` が引くことはない (2026-08-11)。**射影の範囲は live + 過去の全 chunk** であり、生存で絞るのは `WHERE` 句である (2026-08-11 — 03-data-model.md §4 不変条件 7)。
8. 権限の書き込みは常に `.kio` へ。aggregator は投影のみ。
```

不変条件 6-8 は 2026-07-25 の replication 化で追加した ([03-data-model.md §4](03-data-model.md) が正本)。
**aggregator は cache root (`$XDG_CACHE_HOME/kio/aggregator.sqlite`) に置く** — data root ではない。
バックアップ対象外でよく、消しても writer / repair が各 `.kio` から再射影できる (§7.5.2 の backup 例に含めない)。欠損中の `kio search` は source SQLite に fallback しない。

scope registry は共有 `.kio` の正本ではない。フォルダ移動や外部ドライブ切断時は、`scope.json` の `scope_id` を使って再発見または stale 扱いにする (`folder_id` は同概念の旧称であり廃止)。

---

# 4. フォルダごとの `.kio` 運用

`.kio` は基本的に各フォルダに生成される。ただし、空フォルダや未到達フォルダへ先回りして作る必要はない。

推奨:

```text
kio init は現在フォルダの .kio だけを作る
自動子 scope mutation が RC 対象の platform では、kio index が対象ファイルや子scopeを発見した時点で必要な .kio を作る
空フォルダには .kio を作らない
履歴やobjectを持たない .kio は repair / cleanup で整理可能にする
```

子 scope 経路を含む RC platform support policy の正本は [09-mvp-scope.md §1.2](09-mvp-scope.md)
である。現行の子 scope 自動生成は macOS / Linux の retained-handle launcher に限る。Windows では
探索と preview は read-only のまま実行するが、child mutation は
`KIO-E-SCOPE-BOUND-UNSUPPORTED-001` の structured partial failure として返し、子 `.kio` を
作らない。junction / reparse point / directory replacement を pathname fallback で追わず、
retained identity へ全 mutation を拘束できる経路が無い限り fail-closed を維持する。

走査境界の既定 (2026-07-18 確定 — いずれも安全側。緩和は config で明示的に行う):

```text
symlink               lstat 基準で検出し、**追跡しない** (skip + status 表示)。scope 外への
                      参照・循環を構造的に排除する。**判定と open の TOCTOU も閉じる**: 取り込みの
                      open は scope root の dirfd からの相対 open + `O_NOFOLLOW` 相当で行い、
                      open 後の fstat で regular file・同一 device/inode を検証する (lstat 判定後の
                      symlink 差し替えで scope 外 bytes を取り込まない)
hardlink              通常ファイルとして扱う (同一 inode でも path ごとに別実体 — dedup は
                      content hash が自然に吸収する)
外部ドライブ          mount 済みなら通常フォルダと同じ。unmount 中の path は missing 扱い
                      ([03-data-model.md §8](03-data-model.md) の files status)
クラウド placeholder  内容を hydrate しない (placeholder のまま skip + status 表示 —
                      勝手なダウンロードで帯域・容量を消費しない)
権限のないフォルダ    fail-closed: skip + status 表示 (エラーで走査全体を止めない)
hidden directory      OS の hidden 属性・dotfile は通常どおり対象 (除外は .kioignore / Tier A で
                      行う — 隠しであることは機微性の根拠にしない)
system directory      built-in ignore (Tier A 相当) に含め既定除外
.kio 自身             ignore 評価より前に必ず prune (自己再帰の禁止)
VCS リポジトリ root   既定で子 .kio を生成しない。既存子 .kio を grandfather する分岐は置かない ([03-data-model.md §3](03-data-model.md))
```

---

# 5. 物理レイアウト統一

内部正本は immutable manifest CAS と `.kio/objects/normalized_unit_objects/` (full NormalizedUnitObject CAS)
に統一する ([03-data-model.md §2.1](03-data-model.md))。path-named `.kio/objects/normalized_units/` は current
working projection であり、全文 Markdown は unit を決定論的に結合した
view (再生成可能な cache) であり正本ではない。

過去メモにある `.kio/normalized/` は、bootstrap 時の簡略表記または仮想表示パスとして扱う。実装・契約ドキュメントでは、hash ベースの object store を正とする。

```text
truth:
  .kio/objects/manifests/ab/cd/<manifest64>
  .kio/objects/normalized_unit_objects/ab/cd/<unit-object64>

materialized view (cache):
  .kio/objects/normalized_units/ab/cd/<raw64>.<tool64>.g<gen>/
  .kio/objects/normalized/ab/cd/<raw64>.<tool64>.g<gen>.md

virtual view:
  report.pdf.md
```

`<raw64>` / `<tool64>` は論理 hash (`sha256:<64 lowercase hex>`) の digest 部分のみを使う
canonical physical basename である。Windows で無効な `:` を物理名に含めず、論理 hash、JSON、refs、
Evidence URI の identity は変更しない。物理 object は読み書きともにこの canonical 名の 1 表現のみで
解決する — 同一 identity に対する第二の物理表現は存在しない ([03-data-model.md §2](03-data-model.md))。
これは物理ファイル名の portability 規約であり、object hash 算出・論理 identity の変更や
`kio_format_version` の MAJOR bump ではない ([03-data-model.md §2 / §8.1](03-data-model.md))。

---

# 6. 検索バックエンド統一

MVP の標準全文検索バックエンドは SQLite FTS5 とする。Vector は sqlite-vec を標準とする。


> **リスク注記 (sqlite-vec)**: sqlite-vec は v0 系で API 未安定、ANN index を持たない全件 brute-force KNN である。M3-1 の性能目標 (20 scopes / 合計 10 万 chunk で p95 < 5 秒、[09-mvp-scope.md §4.1](09-mvp-scope.md)) は brute-force で達成可能な規模であり、text fallback ([05-runtime.md §1.1](05-runtime.md)) と合わせて現在の標準として維持する。

```text
MVP:
  MetadataStore = SQLite
  TextSearchBackend = SQLite FTS5
  VectorSearchBackend = sqlite-vec

```

---

# 7. Purge の保証範囲

`purge` は、Kio 管理下の object store (本文 bytes と派生 artifact — manifest object 含む)、index、pack、cache (`~/.cache/kio/open/` の一時展開を含む — [05-runtime.md §3.5](05-runtime.md))、
および Kio 自身のログ (`.kio/logs/access.jsonl`、`~/.local/share/kio/logs/` の
events / errors / metrics) から対象ファイル由来の情報を削除する操作である。
**snapshot DAG (commit / tree object) は書き換えない** — tree entry のメタデータ (path, raw_hash) は
履歴に残る (正本 [05-runtime.md §3.5](05-runtime.md))。
ログについては、**当該 scope の scope_id を持ち**対象の raw_hash / path / query を含む行の削除またはフィールドマスクを行う (device-global log の**別 scope の同一 raw_hash 行には触れない** — per-.kio dedup により同一 bytes が独立 scope に併存し得るため。scope 由来の行は scope_id を必須 field とする — §11.6)。
`redact_logs` デフォルト true (§11.6) の運用では query / path / prompt は元から記録されないため、
実務上のスクラブ対象は主に raw_hash 参照行に限られ軽量である。
purge 自体の実行記録 (`commit_type=purged`、tombstone) は監査可能性のため残す ([05-runtime.md §3.2](05-runtime.md))。

ただし、OS backup、Time Machine、クラウド同期の過去版、外部 export、ユーザーが手動コピーしたファイル、Kio 外のログまでは Kio 単体では保証しない。

UI 文言は、過剰な保証を避ける。

```text
推奨:
  Kio 管理下の本文と派生物を全履歴から削除 (ファイル名と存在の記録は履歴に残ります)

避ける:
  世界中のすべてのコピーを完全削除
```

`purge` は必ず次を要求する。

```text
影響範囲 preview
理由入力
明示確認
対象の削除 (raw / prepared / image / normalized / chunk / embedding — 共有派生は live 参照 0 のみ、
  SQLite 行 (chunks / chunk_config_generations / chunk_publications / chunk_vec / image_vec / embeddings / FTS) と、
  chunks.jsonl の**対象 chunk_id を参照する creation 行・publication event 行の全部**を含む。
  正本一覧は [05-runtime.md §3.5](05-runtime.md))
pack / cache / index rebuild
Kio 自身のログのスクラブ (該当行の削除またはマスク) と、その完了有無の結果表示
復元不能な最小 tombstone
```

---

# 7.5 `.kio` の整合性検証とバックアップ

「`.kio` 喪失は復旧不能」(§3 不変条件 3) である以上、破損の検出手段とバックアップ手順を
仕様として持つ。

## device 全体の修復 (`repair all` / `repair replica`)

`kio repair all` (`kio repair -a`) と `kio repair replica` (`kio repair -r`) は、CWD に依存せず
scope registry の `indexed=true` 全行を対象とする。検索参加を無効化した
`participates_in_global_search=false` の scope も、device 上に存在する indexed scope なので対象に含む。
registry は探索入力であり truth ではないため、各行は実際の `.kio/scope.json` と `scope_id` を照合する。
到達不能な行と identity / path が一致しない stale 行 (`KIO-E-REGISTRY-STALE-001`)、複数 live clone は
黙って無視せず `failed_scopes` に記録し、自動 prune / dedupe は行わない。registry 自体を開けない場合に
current scope だけへ縮退して「全体修復」と報告してはならない。例外として、selected scope の
`kio_format_version` 不一致は aggregate failure に変換せず、全 scope の version gate を device replica の
reset や scope 修復より先に完了し、`KIO-E-STORE-VERSION-001` / exit 8 で command 全体を停止する。

- **`repair replica` / `-r`**: 各 scope の `.kio/.lock` を個別に取得し、active purge journal が無いことを
  確認して、既存 `.kio/index/sqlite.db` から `aggregator.sqlite` へ完全射影する。source の HEAD / CAS /
  SQLite は変更しない。source SQLite が無い・破損している scope は失敗として残し、`repair all` を案内する。
- **`repair all` / `-a`**: 各 scope を `verify-objects` (prune 無し) → `rebuild-db` → replica 完全射影の順に
  修復する。raw object の working-tree からの回収と repaired commit は既存 fsck 規則どおり許可するが、
  `--prune-orphans` と `registry-prune` は確認を要する破壊操作なので含めない。device 全体を対象とする
  呼出しで予期せぬ外部通信や課金を起こさないため、新規 online send と既存 Batch job の provider poll /
  remote cleanup を禁止し、不足 enrichment は既存の offline task 規則に従う。

registry connection は対象一覧を owned snapshot として読み終えてから閉じ、その後に scope lock を
決定的順序で 1 個ずつ取得する (registry → scope の lock を同時保持しない)。exact-current version gate
通過後に発生した scope 固有 failure は残りを中断せず、全成功 0 / 部分成功 3 / 全失敗は
[06-cli-spec.md §7](06-cli-spec.md) の同一理由昇格・retryability 規則で返す。いずれも冪等であり、
2 回目は同じ正本から同じ完全状態へ収束する。

## 7.5.1 kio repair verify-objects (fsck 相当)

```bash
kio repair verify-objects
```

- `objects/` 配下の全 CAS object (raw / prepared / image / normalized_unit_object / chunk / embedding / manifest / toollock / tree / commit) を [03-data-model.md §8.1](03-data-model.md) の per-type algorithm で検証し (embedding は vector 長・有限値・vector digest も — 03 §8.1)、
  保存パス・参照 hash と照合する。chunk は object bytes の content hash ではなく semantic identity hash
  と fan-out key、さらに exact `text` / `text_hash` / normalized span を照合する
  ([03-data-model.md §8.1](03-data-model.md))。**embedding も chunk と同じく content hash ではなく identity hash と照合する**
  — 保存 key は「この vector が何の vector か」(target / profile / context) の hash であり、bytes の hash ではない。
  したがって「別の identity の下に置かれた object」は identity 再計算でしか検出できず、
  **vector 本体の bit flip は末尾の vector digest でしか検出できない** (保存 key は本体について何も言わない)
- path-named normalized working projection の unit bytes は content hash を持たないため hash 検証対象外とし、
  current loader が CAS から再生成できる cache とする。**manifest object
  (objects/manifests/ — [03-data-model.md §2.1](03-data-model.md)) は content-addressed であり再 hash 検証の対象**:
  各 tree entry の `normalize.manifest_hash` が実在する manifest object を指し、**かつ当該 manifest の
  (raw_hash, tool_profile_hash, gen) が entry 側と一致する**こと (hash が正しいだけの別 instance
  manifest への誤配線検出) (**tombstone / erase
  receipt が説明する purge 済み raw の entry を除く** — 下記 dead terminal 規則。purge は manifest object を
  削除するが tree は書き換えないため、この例外なしには正規 purge 直後の store が必ず corruption になる)、
  manifest の各 done entry が required non-null `unit_object_hash` を持ち、当該 `normalized_unit_object` CAS の
  JCS re-hash、full schema、unit_key / raw_hash / prepared_hash / tool_profile_hash / gen の manifest との一致を
  検査する。failed entry は explicit null でなければならず、field 欠落・done null・failed non-null は
  current-schema corruption として fail-closed にする。tree `manifest_hash` からこの closure を辿るため、
  `--at` / historical rebuild / Evidence Pointer は mutable projection を参照しない。HEAD tree の entry については
  作業コピー manifest.json の canonical JCS hash が一致することも検査する
  (不一致 = 破損ではなく「**未 finalize の進行状態**」として incomplete (exit 3) — manifest finalize と
  次回 snapshot の間のクラッシュ窓で正常に生じる。次回 index / batch resume が同期する。corruption と
  するのは manifest object 自体の再 hash 不一致のみ)
- canonical tag ref (`refs/tags-v1/tag-*`) と `names.jsonl` (論理名の truth —
  [03-data-model.md §2](03-data-model.md)) は**全行**を検査する: 各行の schema、`digest64` ↔
  `logical_name` の対応 (digest 再計算)、torn tail (最終の不完全行のみ切詰め — 途中の malformed 行は
  corruption)、各 canonical ref ↔ 最終有効行の対応 (03 §2 と同一規則)。対応行の無い canonical ref は
  corruption (ref の無い names 行は tag 削除後の残存として正常)
- SQLite index は検証対象外 (破損時は `rebuild-db` で再構築可能なため)。cursor replay の query vector は
  source SQLite に置かない device-local file cache であり、読取り時に digest を再検証する。欠落・破損の影響は
  cursor 拒否のみ ([04-pipeline.md §4.3](04-pipeline.md))

破損検出時の挙動:

```text
1. working tree に同一ファイルが現存し、再計算 raw_hash が一致
   → re-ingest で object を復元し、commit_type=repaired の commit を記録
     (復元した raw object は GC 対象外、05-runtime.md §2.6)
2. 復元手段なし
   → missing として errors.jsonl に KIO-E-STORE-CORRUPT-001 を記録し、
     (normalized unit の done `unit_object_hash` CAS 欠落も同様 — same-gen 再生成は行わない (unit object は
      immutable であり、非決定的な再生成は過去 commit の内容差し替えになる)。**現行 schema の
      store** の復元は backup restore、または明示の新 gen (`kio reindex --regenerate`) で行う。
      pre-contract development store はこの経路の入力ではなく、source から clean recreate する)
     影響を受ける commit hash の bounded 一覧と
     `external_pointers_may_be_affected=true` を表示する。Evidence Pointer は self-contained で
     registry がないため、存在しない pointer 一覧を推測・捏造しない
3. exit code: 破損 0 件 または 全件復元 = 0 / missing 残あり = 3
```

purge との整合: validated tombstone (lifecycle の event が purged / retired の**いずれでも** — retire は
event を削除せず監査を残すため説明能力を保つ) または erase receipt (non-public — 用途列挙は [08-evidence-pointer-spec.md §4.2](08-evidence-pointer-spec.md)) が説明する missing raw と
その derived (chunk・**当該 (raw_hash, tool_profile_hash) 配下の manifest object**) は正常な dead terminal として数え、
corruption にしない。**説明範囲の限定**: tombstone / erase receipt が説明できるのは、当該 purge event の
時点 (当該 purged / erased event の `in_commit`) **以前**の commit が参照する closure に限る — retire 後に再作成・再公開された
object の欠落は corruption とする (古い退役 event が新規破損を隠さない)。**tree 欠落**は
`.kio/gc/shallowed/<commit64>` receipt が説明する場合のみ正常
(shallow — [05-runtime.md §2.2](05-runtime.md))、receipt なき欠落は corruption。receipt-covered bytes は working copy から
自動復元しない。この receipt (= `.kio/gc/shallowed/` の shallow receipt) は public pointer API と re-ingest barrier には使わない (erase receipt の用途列挙は別 — [08-evidence-pointer-spec.md §4.2](08-evidence-pointer-spec.md))。validな`.kio/gc/in_progress`がある間、fsckはmarkerのfrozen commit/treeとreceipt/treeの遷移をstrict検証し、通常corruptionへ誤分類せず`gc_sweep_incomplete` (exit 3)を返す。malformed marker、frozen binding不一致、marker無しreceipt/tree共存はcorruptionであり、自動修復しない。purge journal が active
なら incomplete exit 3。marker 無し missing は ordinary store corruption、malformed / identity-conflicting
receipt も corruption とする。verified raw と marker の共存の正常判定・修復は **canonical final event ([08-evidence-pointer-spec.md §3.1](08-evidence-pointer-spec.md) 手順 5) を基準にする** (marker 単独の末尾では判定しない — 別 marker のより新しい purge を見逃す): 共存が**正常**なのは canonical final event が `retired` の場合
(resurrection — [05-runtime.md §3.5](05-runtime.md))。canonical final event が `erased` のまま verified raw が
存在する場合は raw を正とし、**その再 publication commit (canonical final event の `in_commit` を
ancestor に持つ ref 到達可能な commit) が存在するときに**、locked repair / 次の locked mutation で
`retired` event を append して整合させる。**canonical final event が `purged` (tombstone) なのに
verified raw が存在する場合**: canonical final purged event の `in_commit` を ancestor に持つ
ref 到達可能な再 publication commit が存在するなら、crash した resurrection の完遂として `retired` を
append して整合させる ([05-runtime.md §3.5](05-runtime.md) の補完規範と同一の因果条件)。**存在しなければ
incomplete purge として exit 3 で報告する** (retired を append しない —
purge 済み内容を fsck が復活させない。回復は同一対象への `kio purge --raw-hash` の再実行で冪等に
完遂できる — [09-mvp-scope.md §5.2](09-mvp-scope.md) の再 purge 規範。報告にはこの誘導を含める) (**receipt は除去しない** — 除去すると旧 commit が参照する
manifest 欠落を説明するものが消える。**commit がまだ無い場合 — snapshot finalize 前の crash — は
「未 finalize の進行状態」として incomplete (exit 3) とし、append しない** —
[05-runtime.md §3.5](05-runtime.md) の因果条件と同型)。

erase receipt の validation は schema_version で分岐する。**v2 (events[])**: strict schema / leaf
identity に加え、各 event が kind 別の必須 field を持つこと (**完全列挙**: purged = `at`・`in_commit`・
`reason`・`actor` / erased = `at`・`in_commit`・`actor` / retired = `at`・`in_commit`・`actor`・
`resurrection_commit`。**すべての current event で** `lifecycle_epoch` (lifecycle counter — 別系統) を必須とし、
purged / erased event では `epoch` (purge counter) も必須とする。**erased は `reason` (5 値 enum —
[02-philosophy.md §2.4](02-philosophy.md) の「どの正当事由で」を erase 後も保存する監査要件)** も必須。
reason は 5 値 enum であり、enum 外の値は corruption とする)、`erased` event の `in_commit` が bounded verified
CAS で ref-reachable な `commit_type=purged` commit を指すこと、当該 commit の `purged_raws` に対象
raw_hash が含まれること ([03-data-model.md §8](03-data-model.md) — `purged_raws` は store format の
初版から必須であり、欠落 commit は存在しない。欠落 = corruption)、各 `at` が canonical UTC でその event
の commit `created_at` と一致し invocation の fixed now より未来でないこと、event 列が有効な遷移
(erased を先頭に erased / retired が交互 — 末尾 event が現況) であること、terminal `retired` の
`resurrection_commit` が ref-reachable で、**直前の erased / purged event の `in_commit` を
ancestor に持つ (= 当該 purge より後の publication である)** ことを必須とする (**resurrection_commit の
verified tree が同一 raw_hash の leaf を含むことを tree 存置時に限り検証する** — auto 型 publication
commit は shallow 化で tree を失い得るため tree 不在時は本検証を省略する。defense-in-depth、
[08-evidence-pointer-spec.md](08-evidence-pointer-spec.md) 手順 8 と同型)。v1 flat (`erased_at` /
`purged_in_commit`) record は current lifecycle schema では受理せず corruption / incompatible format として fail-closed にする。reason 等を合成する conversion は置かない。**tombstone lifecycle にも同じ event 検証を適用する**
(kind 別必須 field・末尾 event 規則・torn / malformed = corruption に加え、**purged event の
`in_commit` が bounded verified CAS で ref-reachable な `commit_type=purged` commit を指すこと・
当該 commit の `purged_raws` への raw_hash membership・各 `at` の commit `created_at` 一致
(canonical UTC・invocation の fixed now より未来でないことも erased 側と同一)・
terminal `retired` の `resurrection_commit` 検証も erased 側と同一に必須**。遷移文法は marker 種別に従う — **tombstone は purged を先頭に purged / retired が交互**
(erased 開始の文法は receipt 専用)。検証失敗の marker は説明能力を持たず corruption とする —
偽 `in_commit` を持つ構造的に正しい tombstone が genuine missing を隠さない)。

**orphan 掃除 (`--prune-orphans`)**: `kio repair verify-objects --prune-orphans` は、どの manifest
からも参照されない orphan prepared / image (公開前 crash の残骸 — [05-runtime.md §3.5](05-runtime.md))
と **descriptor の無い staging root・path と不整合な staging root (descriptor の有無を問わない)・
terminal 化済み (done / failed permanent / abandoned / settled partial — [04-pipeline.md §5.2](04-pipeline.md)) task にのみ対応する staging root
([03-data-model.md §2](03-data-model.md) — 帰属不明の crash 残骸、および
[07-adapter-spec.md §8.3](07-adapter-spec.md) cleanup 失敗の残骸)** を列挙し、locked repair として削除する (確認プロンプト必須。live 参照判定は
purge closure と同一規則)。
ここでは**法務 purge の完結手段だけ**を扱う (purge 完了表示の注記から誘導)。
**拒否条件 (fail-closed)**: 当該 scope に state 0/1 の外部実行 (batch_requests — request_kind 不問)・
pending / running の task・**descriptor を持ち path と整合し、非 terminal (pending / running /
partial / failed retryable) の task に対応する** staging (partial は**再投入可能な failed unit が
残る場合のみ** — 全 unit terminal の settled partial ([04-pipeline.md §5.2](04-pipeline.md)) は
07 §8.3 の cleanup 対象であり blocker にしない) ([07-adapter-spec.md §8.3](07-adapter-spec.md)
— 進行中 task の保全。対応 task を特定できない descriptor つき root は blocker 側に倒す (fail-closed)。
**特定不能の退出経路** (task 記録の喪失許容 — [04-pipeline.md §1](04-pipeline.md) — で blocker が恒久化しないための経路):
(1) descriptor の (raw_hash, tool_profile_hash) 配下に**存在する全て**の normalized instance
(全 gen — descriptor は gen を持たず世代を特定しないため) の manifest で全 unit が terminal
(done / failed permanent) であり、**かつ同 key の state 0/1 batch_requests 行が無い**なら、
terminal 残骸とみなし削除対象へ移す (どの世代の root かを問わず削除が安全になる条件 —
進行中世代があれば「manifest 未 terminal」か「state 0/1 行」のどちらかが必ず塞ぐ。in-flight
信号は喪失許容の task 記録でなく cost-ledger 側を使う)。
(2) それ以外は、同 key の state 0/1 batch_requests 行と pending / running task の不在を lock 下で
検証したうえ、確認プロンプト付きの locked repair として削除できる。
descriptor の無い・path 不整合・terminal 化済み task の残骸は上記の削除対象であり blocker にしない)・未 finalize の
manifest 進行状態・active な purge journal のいずれかが存在する間は、prune を実行せず exit 3
(retryable) で拒否する — **manifest 未確定の正規進行中 prepared / image を orphan と誤認して削除
しない**ため (相 3 collect の入力を消すと再課金・欠落参照になる。終端・完了後に再実行する)。拒否応答には
blocker の種別と対象 (intent_token または 4 組キー) を含め、次操作 (`kio batch resume` /
`kio batch abandon` / journal 回復) を提示する — terminal (state 2/3) の行は blocker にならない。

**purge closure** は raw とその prepared / image / manifest / normalized_unit_object / chunk / embedding の
到達 object、および current projection / view cache を含む。purge / verify の marker による dead-terminal
説明も、この immutable unit-object closure に及ぶ。**purge 済み raw の open cache 残骸**: `--prune-orphans` は、当該 scope で canonical final event が
`purged` **または `erased`** である各 raw_hash について `~/.cache/kio/open/<raw_hash digest64>/` の
残存も検査し、存在すれば同じ locked repair の削除対象に含める ([06-cli-spec.md §1.1](06-cli-spec.md) の
cache publish と起動直前検査の間の crash は、publish 済み cache を除去主体なしに残し得る —
purge 完遂後の平文残存の回収経路。cache は再生成可能な非 truth であり、同一 raw が他 scope に
live で存在しても削除は安全 — 次回の open が再展開する)。**image cache も同様に回収する**:
orphan image の削除時は対応する `~/.cache/kio/open/image/<image_hash digest64>/`
([06-cli-spec.md §1.1](06-cli-spec.md)) を削除対象に含め、当該 scope のどの live manifest からも
参照されない image の cache dir 残存も同じ検査で回収する (crash 窓の残存は raw と同型。**判定は
当該 scope の manifest 列挙による** — 他 scope で live な同一 image の cache を削除し得るが、
cache は再生成可能な非 truth であり次回の open が再 materialize する = raw と同じ削除安全性)。

MVP では手動実行のみとする。

### Read-only unreachable-object inventory との責務分離

Phase 4 milestone 8 の `kio gc --dry-run --prune-unreachable` は `verify-objects` のrepair計画でも、
`--prune-orphans` のpreviewでもない。前者は全physical CAS objectをdescriptor-boundで2回読み、
正本graphから診断分類を返すだけで、lock leaf・scope・cacheを含め一切書き換えない。後者だけが
確認付きlocked repairとしてprepared/image/staging/open-cache lifecycleを削除できる。

したがって inventory の `candidate` 行を `prune_orphans_apply` へ渡すAPI、report再読込み、receipt、
resume state、削除executorは置かない。prepared/imageはinventoryでは常に`inventory_only`であり、
manifest/normalized-unit/embedding/未公開tool-lockのdiagnostic candidateも現行operationでは削除不能である。
分類とschemaの正本は [05-runtime.md §2.7](05-runtime.md) / [06-cli-spec.md §6.2](06-cli-spec.md)。

## 7.5.2 バックアップ運用

正式なバックアップ手段は次の 2 つとし、専用コマンドは MVP では追加しない。

```text
1. .kio ディレクトリごとのコピー (MVP の推奨手段)
   - コピー中に kio が書き込まないこと (.kio/.lock 未取得状態) を確認してから行う。
     **注意: この確認は check-then-act であり、確認とコピーの間の書き込みまでは防げない** —
     コピー中に kio コマンドを実行しないことがユーザー前提。厳密な原子性が必要なら
     filesystem スナップショット (APFS/btrfs 等) 上でコピーする。復元後は
     `kio repair verify-objects` (§7.5.1) を必ず実行して整合を確認する
   - sqlite.db は repair rebuild-db で完全に再構築可能である。cursor replay の query-vector cache は `.kio` 外の device-local file であり、バックアップ対象ではない（喪失時は cursor を拒否し再検索する [04-pipeline.md §4.3](04-pipeline.md)）。ただし**最低保全集合は objects/ と refs/ では
     なく、[03-data-model.md §4.1](03-data-model.md) の truth 区分の全行** (scope.json / config /
     tool-lock / tombstones + erase receipts / chunks.jsonl / access.jsonl を含む) — これらは
     いずれも喪失時復旧不能である
   - **デバイスグローバルの cost-ledger.sqlite は `.kio` コピーに含まれない** — 別途
     `sqlite3 "<Kio data dir>/cost-ledger.sqlite" ".backup <dest>"` (WAL-safe。**必ず実体の絶対パスで
     指定する** — 相対パスはカレントに空 DB を新規作成し「正常にバックアップできた」ように見える。
     例: `sqlite3 "${XDG_DATA_HOME:-$HOME/.local/share}/kio/cost-ledger.sqlite" ".backup /backups/kio-cost-ledger.sqlite"` —
     `<...>` は展開後も絶対パスであること。
     **復元後は `PRAGMA integrity_check` と cost_ledger / batch_requests 両表の存在を確認する**) で
     バックアップし、復元は Kio プロセス非実行中に行い、復元後は
     §5.8 の回復 (reconcile) が完了するまで新規 Batch 投入を行わない ([04-pipeline.md §5.4](04-pipeline.md))。
     **復元した DB は backup 以後の投入記録を失っている** — 復元後の初回回復では、**記録済み provider
     scope と現在構成の各 Batch client の provider_scope_id を合わせた集合**の全ページ一覧
     ([04-pipeline.md §5.8](04-pipeline.md) の confirmed-absent と同じ走査 — backup 後に初使用した
     provider scope を取りこぼさない。どちらにも無い scope は原理的に走査できない) のうち
     `batch_requests` に対応行が無い job / upload を次の帰属規則で報告する (**job の帰属は token 形式では
     なく job metadata の task key 4 組が担う** — [04-pipeline.md §5.8](04-pipeline.md) の帰属規範と同一。
     UUIDv7 token 単独では帰属できない。出力 unit の対応付けに使う custom_id は別層 — 同 §5.8)。
     **「ローカル構成の scope」= 判定時点の scope_registry の行のうち、root_path の `.kio/scope.json` を
     実地検証 (読取 + scope_id 一致) できた scope_id 集合** (registry は cache — 喪失・prune 済みなら
     [05-runtime.md §6](05-runtime.md) の再構築 (ユーザー既知 root での再登録) を先行させる。未再登録
     scope の job は unknown 側に落ち、再登録後の再実行で orphan 候補へ移る — 報告は冪等):
     metadata の task key 4 組が完全に読め、かつ scope_id がこの集合に一致する job は
     orphan 候補として報告し、結果取得 (読み取りのみで安全) と、**他 Kio インスタンスとの provider
     scope 共有がないことを確認した上での**削除を案内する。metadata が一致しない・読めない job と、
     filename の token しか持たない upload は**帰属不能 (unknown) として報告のみ** — 結果取得・削除の
     どちらも案内しない (他インスタンス・他ツール由来があり得る)。自動再投入・自動削除はしない
     (二重課金と orphan 課金の可視化)

```

復元後は `kio repair verify-objects` で整合性を確認する。外部ドライブ・クラウド
ストレージの placeholder file 上の `.kio` は破損リスクが高いため、§4 の境界方針の確定
までは推奨しない。

## 7.5.3 SQLite schema 変更の規約 (rebuild vs in-place migration)

sqlite.db / scope-registry.sqlite / aggregator.sqlite は正本から再構築可能な cache である ([03-data-model.md §4.1](03-data-model.md))。cursor replay の query-vector cache は device-local file であり、SQLite schema migration の対象ではない ([04-pipeline.md §4.3](04-pipeline.md))。
したがって schema 変更のデフォルト経路は **migration を書かず再構築する** こと
(sqlite.db は `kio repair rebuild-db`、registry は各 `.kio` の rescan、aggregator は writer / repair の完全射影)。

**`cost-ledger.sqlite` はこのデフォルトの対象外** — 課金記録と in-flight Batch intent の再構築不可な
運用上の truth である ([04-pipeline.md §5.4](04-pipeline.md))。そのため旧 JSONL importer、`.migrated`
rename、**JSONL cutover 用** marker、列 default/backfill を用いる後方互換 migration は置かない。current
operational action (例: restore-reconcile) の durable marker として `schema_migrations` を使うことは維持する。

起動時は `cost_ledger` / `batch_requests` / `schema_migrations` の **3 表すべて**の table 定義、および
`idx_cost_ledger_month` / `idx_batch_requests_inflight` を canonical DDL と比較する。空の新規 ledger
だけは current schema で初期化できる。既存 ledger が不一致・欠損・読取不能なら、**bytes を保存した
まま一切 rename / ALTER / DROP / import せず fail-closed** にする。エラーは ledger が non-rebuildable
であることと、明示的な operator recovery が必要なことを示す。推測変換や startup の自動修復で課金額・
intent・監査履歴を失わせてはならない。current-schema ledger の torn write / provider crash recovery は
§5.4 / §5.8 の既存の recovery 契約として維持するが、旧 shape を current shape と見なす契約ではない。

`PRAGMA user_version` は互換 reader のために導入しない。derived SQLite cache は schema fingerprint が
current と一致しない既存 DB を無変更で `kio repair rebuild-db` へ誘導し、fresh / missing DB だけを
current schema で作成する。この gate は最初の書込みより前に評価する。

---

# 8. commit_type の固定 enum について

現在の正本では、`commit_type` を `manual / auto / repaired / purged` の4種に閉じる。`migrated` を含む store は current reader の受理対象ではない。

この方針を採用する場合でも、実装では以下を守る。

```text
type に混ぜない情報は actor / source / trigger / metadata に逃がす
metadata には schema_version を持たせる
未知 type を読んだ場合の error message を明確にする
新 type が必要に見える場合は、まず既存 type + metadata で表現できないか確認する
```

current reader は [05-runtime.md §2](05-runtime.md) の値域だけを受理し、legacy enum の読取りや変換を行わない。

---

# 9. Adapter セキュリティ

R23 の Markdownize / Embedding Adapter は Kio 同梱の built-in target のみを実行し、
任意コマンドや任意 URL への差し替えは受理しない。

最低限必要な制御:

```text
allow_network
allowed_scope
max_input_bytes
timeout_seconds
redact_logs
store_request_body = false
store_response_body = false
secret redaction
```

これらの policy の強制モデル (宣言 + 監査であって sandbox 保証ではないこと) は
[07-adapter-spec.md §7.1](07-adapter-spec.md) を正本とする。

オンライン Adapter は、明示 opt-in なしにファイル内容を送信してはならない。opt-in の
単位 (scope × adapter)・寿命・revoke は [07-adapter-spec.md §3](07-adapter-spec.md) を
正本とする。初回スキャン preview でも、network transmission policy を表示する。

---

## 9.1 Incremental Markdownize (要件)

ファイルが更新された場合、Markdownize (OCR を含む) Adapter には **新 raw だけでなく、旧 raw + 旧 normalized Markdown + 変更ヒント** をセットで渡し、変更が軽微なら Adapter が部分更新を返す方式を採用する。MVP〜v1 のプロダクト要件として確定する。

目的:

```text
1. LLM API コスト抑制 (04-pipeline.md §5.4 の cost guardrail と整合)
2. 全文再生成による表記ゆれ・見出し変動を抑制
   → unit_key / chunk / Evidence Pointer の安定性向上
3. 変わっていない unit の再 Markdownize 呼び出しを完全排除
   (embedding は text_hash 一致による再利用で抑制する, 04-pipeline.md §5.5)
```

実装責務の分担:

```text
Kio:
  - 変更検出 (raw_hash 変化 + unit_mapping による変化率算出, 04-pipeline.md §2.2)
  - 発動条件の判定 (capability / 閾値 / 連続回数)
  - Adapter への入力組み立て (旧 raw, 旧 Markdown, hints)
  - Adapter からの fallback_to_full 受信時の full 再投入
  - normalization_run への mode/parent_run_id/changed_unit_keys の記録

Markdownize Adapter:
  - capabilities = ["incremental_update"] の宣言
  - incremental 入力を受け取って updated_units / unchanged_unit_keys を返す
  - 軽微でないと判断したら fallback_to_full=true を返す
```

Adapter が `incremental_update` capability を宣言しない場合は、Kio は常に full モードで Adapter を呼ぶ。これは current capability contract の縮退であり、旧 store / object reader の後方互換ではない。

詳細仕様: [04-pipeline.md §2, §3](04-pipeline.md), [07-adapter-spec.md §8](07-adapter-spec.md)

設定上書き例 (`.kio/config.toml`):

```toml
[markdownize.incremental]
enabled = true
threshold = 0.30
max_consecutive = 5
```

---

# 10. 現行仕様の索引

以下の仕様は既に正本 spec に統合済みである。着手前に該当節が凍結ゲート ([09-mvp-scope.md §6.2](09-mvp-scope.md)) を通過していることを確認する。

```text
object store / snapshot DAG      → 03-data-model.md
Evidence Pointer schema          → 08-evidence-pointer-spec.md
永続ストア一覧 (SQLite/file 境界) → 03-data-model.md §4.1
SQLite schema (index / registry) → 04-pipeline.md §4 / 10-operations.md §3
object / manifest schema         → 03-data-model.md §8
ingest / markdownize / snapshot  → 04-pipeline.md
restore / resume-retry           → 05-runtime.md / 04-pipeline.md §5.7
検索評価規約 / 評価指標定義        → 09-mvp-scope.md §4.3
done criteria                    → 09-mvp-scope.md
```

---

# 11. 横断規約 (cross-cutting contracts)

複数のドキュメントで部分的に触れられている規約事項を一元化する。各個別ドキュメントの記述はこの章を **正本** として参照する。

## 11.1 エラーコード namespace

すべての error は `KIO-E-<DOMAIN>-<SUBDOMAIN>-<NNN>` 形式の **error_code** を持つ。`error_kind` などのフリーテキストはユーザー向け表示専用で、機械判定には `error_code` を使う (明示例外 = manifest `units[]` / Adapter 出力 `failed_units` の `error_kind` — [04-pipeline.md §5.3](04-pipeline.md) の閉 enum であり、unit 単位の retry 可否判定に使う)。

```text
DOMAIN:
  BATCH    バッチ処理 (markdownize / embedding / etc.)
  INDEX    インデックス更新
  REPAIR   device / scope 修復の集約
  SEARCH   検索 (FTS / vector / hybrid)
  COMMIT   commit / snapshot / restore
  GC       garbage collection
  PURGE    purge 操作
  EVIDENCE Evidence Pointer 解決 / verify
  REGISTRY scope registry (live clone 重複・退役 — [§3])
  ADAPTER  Adapter ロード・実行
  EMBED    embedding profile / modality 検証 (KIO-E-EMBED-MODALITY-001 — [03-data-model.md §7](03-data-model.md))
  CONFIG   config / schema / 設定
  STORE    object store / fs IO
  AUTH     認証・認可
```

例: `KIO-E-BATCH-NET-001`, `KIO-E-SEARCH-VEC-INCOMPAT-001`, `KIO-E-SEARCH-VEC-UNAVAIL-001`, `KIO-E-SEARCH-VEC-UNAUTHORIZED-001`, `KIO-E-COMMIT-SHALLOW-001`, `KIO-E-PURGE-NOT-FOUND-001`, `KIO-E-PURGE-JOURNAL-ACTIVE-001` (未完了 purge journal / epoch 不変違反による**読み取り系** preflight の拒否 (書き込み系は journal 回復を再開)。**restore の rename 後再検査が対象を closure に含む active journal を検出した場合の publish 後巻き戻し終端にも用いる** — retryable、exit 3、[05-runtime.md §3.5](05-runtime.md)), `KIO-E-COMMIT-RESTORE-CONFLICT-001` (restore の publish / 巻き戻しにおける no-replace 競合・dev/inode 不一致・退避 / 隔離の同名残存 — context に閉 enum `conflict_kind`・`retry_disposition` (transient / manual_action) と両者の所在を含む。retryable、exit 3、[05-runtime.md §3.5](05-runtime.md)), `KIO-E-ADAPTER-APPROVAL-CONFLICT-001` (承認 publish 直前の CAS 不一致 — 並行 revoke による pending 除去・再承認が必要。exit 5、[07-adapter-spec.md §3](07-adapter-spec.md)), `KIO-E-ADAPTER-SPECVER-001` (spec_version 不一致 — invalid_input / 非再試行、[07-adapter-spec.md §8.1](07-adapter-spec.md)), `KIO-E-STORE-PATH-001`, `KIO-E-STORE-CORRUPT-001`, `KIO-E-STORE-VERSION-001` (§11.5 — current `KIO_FORMAT_VERSION` と完全一致しない store を全 command で拒否、exit 8), `KIO-E-SEARCH-SCOPE-ALL-FAILED-001`, `KIO-E-SEARCH-CURSOR-001`, `KIO-E-INDEX-REBUILDING-001`, `KIO-E-EVIDENCE-SCOPE-UNREACHABLE-001`, `KIO-E-ADAPTER-CONTRACT-001`、`KIO-E-PURGE-REPLICA-001` (purge 後の device replica 再射影に失敗 — 本文が cache root に読める状態で成功と報告しないための fail-closed 終端。exit 1、[05-runtime.md §3.5](05-runtime.md))、`KIO-E-CONFIG-OFFLINE-URL-001` (`execution_mode = "offline_api"` の Adapter に loopback リテラル (`127.0.0.1` / `localhost` / `[::1]` / UNIX domain socket) 以外の `url` が宣言されている — tool-lock materialize / adapter 登録時に検証。exit 2、[07-adapter-spec.md §3](07-adapter-spec.md))。各 code の定義箇所は該当 spec (06-cli-spec.md §8 に一覧と参照先) を参照。

device-global repair の scope 集約 code は `KIO-E-REPAIR-PARTIAL-001` と
`KIO-E-REPAIR-ALL-FAILED-001` とする ([06-cli-spec.md §7](06-cli-spec.md))。
registry 行が on-disk の scope identity / path と一致しない場合は
`KIO-E-REGISTRY-STALE-001` とする。

各 spec が定義した個別エラー (04-pipeline.md / 05-runtime.md / 06-cli-spec.md 等) はこの namespace に従う。新規 code 追加は本書および該当 spec の更新を伴う (破壊的変更扱い)。

## 11.2 CLI exit code

Kio のすべての CLI コマンドは以下の exit code を返す。

```text
0   成功 / 全 up_to_date
1   汎用 failure (詳細不明)
2   invalid usage / config 不正 / schema validation 失敗
3   retryable な失敗が残っている (部分成功・全体 retryable を含む — [06-cli-spec.md §7](06-cli-spec.md))
4   permanent な失敗のみが残っている (全失敗 permanent、および settled partial
    (部分成功 + 残り全 permanent — 04-pipeline.md §5.2) を含む — 再試行で進展しない)
5   auth_error (user action 必要)
6   budget_exceeded により paused
7   user 中断 (SIGINT/SIGTERM)
8   incompatible profile / format version
9   confirm 拒否 (purge 等の確認プロンプトで no)
```

スクリプト連携はこれらを参照する。コマンド固有の補足は各 sub-command が docstring に明記する。

dead pointer (tombstoned / not_found) は `4`、**scope_unreachable のみは retryable の `3`** (再接続・registry 再登録で回復可能 — [08-evidence-pointer-spec.md §4.3](08-evidence-pointer-spec.md))、tool_profile 不一致による chunk 解決不能は `8` に割り当てる (詳細: [06-cli-spec.md §7](06-cli-spec.md))。

## 11.3 設定ファイル schema validation

すべての設定ファイルは JSON Schema (TOML は JSON 等価表現に変換して同 schema で validate) を持ち、CLI 起動時に schema-driven validation を行う。schema は Kio 本体に同梱する。

```text
~/.config/kio/tools.toml          → schemas/tools.schema.json
~/.config/kio/config.toml         → schemas/user-config.schema.json
.kio/config.toml                  → schemas/folder-config.schema.json
.kio/scope.json                   → schemas/scope.schema.json
.kio/tool-lock.json               → schemas/tool-lock.schema.json
.kio/manifest.json (簡易管理時)    → schemas/manifest.schema.json
```

validation 失敗は exit code 2 で停止し、`KIO-E-CONFIG-SCHEMA-001` を返す。schema は semver で版管理する。format change は新しい current version を選択するが、migration / old-version read-only / dual-read を定義しない (§11.5)。

`scope.schema.json` は少なくとも次の key を定義する: `scope_id` (required)・子 `.kio` リンク ([03-data-model.md §2](03-data-model.md))・`scan_approval` (optional — §1 の取り込み承認記録。required field は §1 の記録一覧と一致)・`approvals[]` (optional — adapter 単位の network opt-in。要素の required field = scope_id / tool_id / execution_mode / tool_profile_hash / approved_at / approval_method / status (`active` | `revoked`)、status=revoked の行は revoked_at も必須 — [07-adapter-spec.md §3](07-adapter-spec.md))・`approval_pending` (optional — 承認書込順の pending intent、[07-adapter-spec.md §3](07-adapter-spec.md)。**単一 object (配列にしない — 承認操作は `.kio/.lock` で直列化され並存しない)**。存在する場合の required field = scope_id / tool_id / execution_mode / tool_profile_hash / **approved_at / approval_method** (公開行の監査値 — self-heal がそのまま publish する))・`approvals_initialized` (optional boolean — 初回承認の行 publish、pending 除去・行 revoked 化を実行した revoke と同一 atomic write で true 化する消費済み marker ([07-adapter-spec.md §3](07-adapter-spec.md))。true かつ approvals[] 空 = 初回例外の消費済み (台帳喪失・revoke 後を含む) として blanket 自動 materialize を fail-closed にする、07 §3)。**`approval_pending` key 全体の不在は valid で pending intent 無しを表す。一方、存在する pending の required field 欠落・型不正は schema error / fail-closed** であり、self-heal、locked cleanup、監査値の補完の対象にしない。`approvals[]` が存在する場合の各行も同じく strict に検証し、`status` 欠落行は送信を許可しない (既定値で active と読む経路は持たない)。**未知 key は schema error** (fail-closed)。この schema validation は `kio_format_version == KIO_FORMAT_VERSION` の完全一致判定より後に走る。missing / non-string / malformed / older / newer / unknown を含む non-current store は schema validation へ進めず `KIO-E-STORE-VERSION-001` / exit 8 で全 command が拒否する。`approvals[]`・`approval_pending`・`approvals_initialized` の**全て**を欠く current-version scope.json は valid であり、欠落 = 当該承認なしとして扱う。

`folder-config.schema.json` は `[chunking].unicode_version` を **required** とする (省略不可・default なし — `kio init` が実装同梱の UCD 版 (現在の既定 = 17.0.0) を明示記録する、[03-data-model.md §5.3](03-data-model.md) / [06-cli-spec.md §1](06-cli-spec.md))。これを欠く `.kio/config.toml` は schema error (exit 2) とする — required field に既定値の代替経路は持たない。`[markdownize].bbox_annotation` (boolean、既定 true — [07-adapter-spec.md §5.2](07-adapter-spec.md)、値は tool_profile_hash に畳み込む) も本 schema の正式 key として定義する。

`tools.schema.json` は adapter ごとの `pricing` を定義する: **key = billable_units の kind 閉 enum (pages | tokens_in | tokens_out — [07-adapter-spec.md §4](07-adapter-spec.md))、値 = 有限・非負の USD 単価 (REAL)、未知 key は schema error**。**billable を宣言する Adapter ([07-adapter-spec.md §5.5](07-adapter-spec.md) 条件 6) は、AdapterProfile の `billable_kinds` (報告し得る kind の閉集合の宣言 — [07-adapter-spec.md §4](07-adapter-spec.md)) の全 kind が `pricing` に被覆されること (pricing keys ⊇ billable_kinds) を送信前に検査する (欠落は config error — fail-closed)**。billable 宣言 Adapter の profile required には `reject_billing` (閉 enum — [07-adapter-spec.md §4](07-adapter-spec.md)) も含める (**AdapterProfile の実行時受入規範であり、tools.schema.json の検証対象ではない** — 欠落は 07 §4 のとおり fail-closed "billable" として読む)。終端時に初めて解決不能と判明した場合の縮退は [04-pipeline.md §5.4](04-pipeline.md)。

`user-config.schema.json` は device cap (`[budget]`、[04-pipeline.md §5.4](04-pipeline.md)) を含む。**log 保持の正規 key = `[observability] retention_days`** (整数 1〜3650・既定 30 — §11.6 の「config 上書き可」の実体。device logs (events / metrics / errors) と scope-local `.kio/logs/access.jsonl` の双方に適用する)。

## 11.4 時刻・タイムゾーン

すべての永続データ (commit timestamps, normalization_runs, access_events, snapshot lineage 等) の時刻は **UTC ISO8601 拡張形式 + suffix `Z`** に固定する。**例外 = SQLite ストアの内部時刻列** (cost-ledger.sqlite の recorded_at / job_create_started_at / stale_after_at / completed_at / created_at — [04-pipeline.md §5.4](04-pipeline.md)): SQL での比較・期限演算のため **UTC epoch ミリ秒の INTEGER** を正とする (JSON / JSONL / UI 境界へ出す際に ISO8601+Z へ変換する)。**暦の演算も UTC で行う** — `cost_ledger.month` ('YYYY-MM') は `recorded_at` の UTC 暦月から導出し、[04-pipeline.md §5.4](04-pipeline.md) の剪定の「前月以前」判定も UTC 暦月の月初 epoch ms を境界とする (local TZ は UI 表示限定 — 下記)。

```text
正:   2026-04-25T12:00:00Z
正:   2026-04-25T12:00:00.123456Z
誤:   2026-04-25T12:00:00      (TZ 欠落)
誤:   2026-04-25T12:00:00+09:00 (local 表記)
```

ユーザー向け UI 表示時のみ local TZ に変換する。snapshot lineage の順序判定は UTC タイムスタンプを使い、Lamport/HLC 系の論理時計は v0 では採用しない (採用判断は v2 の同期設計で別途。経緯: 旧 research/synchronization.md — git 履歴)。

## 11.5 current-format boundary

current reader は `KIO_FORMAT_VERSION` と**完全一致**する string の `kio_format_version` だけを受理する。この判定は、すべての incompatible store に安定した `KIO-E-STORE-VERSION-001` / exit 8 を返すためだけに current schema validation より先に行う。missing、non-string、malformed、older、newer、unknown を含む任意の non-current 値は拒否し、いかなる command も拒否前に store bytes を変更してはならない。

この境界は reader / search / repair / historical の全 command に適用する。migration reader、old-version reader、read-only compatibility mode、best-effort 例外はない。multi-scope search は selected scope が non-current と分かった時点で停止し、その scope を `excluded_scopes` に記録せず、version fallback を記録せず、healthy scope だけの partial result を返さない。incompatible derived SQLite も byte-for-byte で残し、`repair rebuild-db` は validated current commit/tree/manifest/CAS truth からだけ再構築し、old row を読まない。

current-format の recovery は別契約である。fresh / missing derived SQLite は current schema で初期化してよく、torn write や current-format lifecycle / purge の失敗には定められた fail-closed recovery を維持する。`cost-ledger.sqlite` は non-rebuildable truth であり、old / missing / incompatible shape は import、rename、ALTER、推測変換をせず保存して拒否する。canonical digest-only name、Unicode NFC/case folding、Windows write/read/hash verification の portability も維持するが、legacy physical-name fallback は維持しない。

format change が新しい `KIO_FORMAT_VERSION` を選択した時点で、旧 version は直ちに non-current となる。semver / CHANGELOG は変更を記録するが、migration、dual-read、read-only compatibility を認可しない。独立して version を持つ他の interface はそれぞれの validation 規約に従う。

撤去の追跡は [tasks/pre-release-legacy-removal.md](../tasks/pre-release-legacy-removal.md) にある。

**tree schema v2/v3 (2026-07-18 確定)**: tree entry へ `normalize.manifest_hash`、tree object へ
`chunking_config_hash` (v2) と `chunk_set_hash` (v3 — 公開 chunk 集合の digest) を追加した
([03-data-model.md §8](03-data-model.md)) — hash/identity 規約の変更だが、
**実装・store 公開前の schema 確定であり MAJOR bump ではない**。current tree ではこれらの field は必須であり、欠落は legacy semantics へ縮退せず corruption / incompatible format として fail-closed にする。Step 1-2 実装の tree hashing は v2/v3 対応の rework が必要 ([09-mvp-scope.md](09-mvp-scope.md))。

**Adapter 入出力の `spec_version` bump 規約**: `tool-lock.json` の `spec_version` および Adapter 入出力 schema ([04-pipeline.md §3.1](04-pipeline.md)) の `spec_version` は単調増加の整数とする。bump するのは、フィールドの削除・必須化・意味変更など**旧 Adapter が誤動作しうる変更のみ** (MAJOR 相当。該当 spec と CHANGELOG への明示記載必須)。optional フィールドの追加では bump せず、代わりに Adapter は未知フィールドを無視しなければならない (MUST ignore unknown fields)。不一致時の挙動は分業する: Adapter 側は `invalid_input` として失敗する ([07-adapter-spec.md §8.1](07-adapter-spec.md))。**full fallback が有効なのは incremental capability だけが非互換な場合に限る** — spec_version 自体の非互換は full で呼び直しても同じ拒否を再生するため、当該 online Adapter のタスクを failed permanent (Adapter 更新が必要) とし、同梱 deterministic Adapter のベースラインは影響なく継続する ([07-adapter-spec.md §8.1](07-adapter-spec.md) と同旨)。index 全体の停止を引き起こさないという保証は、このベースライン継続が担う。

current `commit_type` の値域は [05-runtime.md §2](05-runtime.md) を正本とする。

## 11.6 観測 (observability)

`logs/access.jsonl` 以外に、以下の構造化ログを `~/.local/share/kio/logs/` に出す。

```text
events.jsonl       重要イベント (commit, gc, purge, schema migration)
metrics.jsonl      数値メトリクス (任意の interval、デフォルト1時間に1行)
errors.jsonl       error_code 付きの全エラー
```

各行 JSON で次のフィールドを必須とする:

```text
ts        UTC ISO8601 (§11.4)
level     debug | info | warn | error
code      error_code (KIO-E-) / event_code (KIO-EV-) / metric_code (KIO-M- — [05-runtime.md §7](05-runtime.md))
component batch | search | commit | gc | ...
message   人間可読な短文 (非機微テンプレートに限る — query / path / prompt 等の値は context 側に
          置いて redaction を通す。自由文へ機微値を埋め込まない)
context   必須 field (空 object 可) — 値は JSON object (tool_profile_hash, commit_hash, raw_hash,
          scope_id 等。file_id は廃止済み識別子のため使わない。
          **scope 由来の行は context.scope_id を必須とする** — purge の対象化キー (§7)。
          複数 scope に跨る行 (横断検索 metric 等) は scope_id を持たない — **そのためこれらの行には
          raw_hash / path / query 等の対象由来値を記録しない** (purge の対象化が届かないため。
          必要なら行を scope 別に分割する))
```

ログのローテーションは日次、保持は 30 日 (config 上書き可 — 正規 key = `[observability] retention_days`、§11.3)。**scope-local の
`.kio/logs/access.jsonl` も同じ規範の対象とする** (日次 rotation + 保持日数は同 config・既定 30 日 —
無操作でも検索対象であり続ける scope の unbounded 成長を防ぐ。purge の scrub は**全保持世代**に
適用する — rotation は scrub の対象範囲を狭めない。access_events の正本性は保持期間内の記録に
ついて成立し、期間経過後の世代破棄は監査要件に応じ config で延長して調整する)。`redact_logs` の
デフォルトは **true** であり、`[adapter.policy]` に限らず observability ログ
(events / metrics / errors) と access.jsonl の全域に適用される。true の場合、
`context` の `query`, `path`, `prompt` 等の機微フィールドを、nested な値も含めて同一 policy でマスクする (`message` は上記のとおり非機微テンプレート限定 — マスク対象の値を含めない)。
false への変更は明示設定のみで行える。

## 11.7 命名リネーム表 (旧 → 新)

過去メモから現行設計への移行で発生した renaming を一覧化する。実装者はこの表を grep して旧称残置を排除する。
(出所列の research/*.md は 2026-07-18 に docs から撤去済み — git 履歴で参照可)

```text
旧称                            | 現行                                | 出所
-------------------------------- | ----------------------------------- | ----
folder.json                      | scope.json                          | research/kio.md §6
folder_id                        | scope_id                            | 10-operations.md §3
normalized_hash                  | (廃止)                               | research/hash.md §9
canonical_text_hash              | (廃止)                               | research/diff.md §8
canonical_hash                   | (廃止)                               | research/diff.md §17
markdown_hash                    | (廃止)                               | research/diff.md §3
Normalized-Hash: <Markdown header> | Tool-Profile-Hash: <Markdown header> | research/read_only.md §2
.kio/normalized/<path>.md        | manifest CAS → normalized_unit_object CAS (正本。`normalized_units/` は current projection) | research/kio.md §11
unit_id                          | unit_key / unit_ref                 | 03-data-model.md §2.1
last_indexed_git_commit          | (廃止: Git 連携は持たない)             | research/kio.md §10
output_hash (in normalization_runs) | (廃止)                            | research/hash.md §3
cost-ledger.jsonl (+ -reservations / -reclaimed / .lock) | cost-ledger.sqlite (cost_ledger / batch_requests / schema_migrations の 3 表) | 04-pipeline.md §5.4
```

## 11.8 推奨 Reading Path

Reading Path の正本は [README.md §1](README.md)。docs/ 直下のファイル名の数字プレフィックスがそのまま読む順番であり、本書で別の順序を定義しない。

---

# 12. RC draft artifact の検証と導入

本節は、`draft-release` workflow が生成する Actions artifact と、公開済み GitHub Release asset の
双方の運用正本である。公開済み `v0.1.0-rc.1` archive 内の `OPERATIONS.md` は immutable な
published artifact bytes であり、本修正では差し替えない。本節の更新は main と将来の candidate に
適用する。
RC version の正本は workspace package version であり、現在の候補は `0.1.0-rc.3` とする。
platform support policy は [09-mvp-scope.md §1.2](09-mvp-scope.md) だけを正本とし、
macOS / Linux は `supported`、Windows は `experimental` とする。

`draft-release` workflow が生成する per-target Actions artifact は、同じ target 用の次の files を
一組として保持する。

```text
kio-0.1.0-rc.3-<target>.tar.gz
kio-0.1.0-rc.3-<target>.checksums.json
smoke.json
```

`smoke.json` は workflow run 内部の smoke evidence であり、GitHub Release asset として公開しない。
公開済み [`v0.1.0-rc.1` GitHub pre-release](https://github.com/ttokunaga-ja/kio/releases/tag/v0.1.0-rc.1)
の公開 asset は、`aarch64-apple-darwin` / `x86_64-unknown-linux-gnu` /
`x86_64-pc-windows-msvc` の 3 target それぞれの archive + checksums sidecar、合計 6 files のみである。

archive には展開後の `bin/kio` (`bin/kio.exe` on Windows)、`LICENSE.md`、
`NOTICE.txt`、`TRADEMARKS.md`、本書を写した `OPERATIONS.md`、canonical provenance、
CycloneDX SBOM、dependency / license inventory、dependency audit receipt、内部 checksum
manifest を含める。外部 `checksums.json` は archive 自身の SHA-256 と内部の binary / SBOM /
provenance / checksum digest を同時に束縛する。archive 自身の digest を archive 内へ自己参照で
埋め込まず、外部 sidecar を一組として検証することで循環を作らない。

`dependency-audit.json` は固定版 `cargo-deny` による offline の bans / licenses / sources 検査の
receipt である。P3 draft は advisory database を使用せず、provenance と receipt の
`advisory_database` は `null` とするため、脆弱性 advisory scan を実施したという主張はしない。
将来 advisory 検査を追加する場合は、database revision を provenance へ固定する必要がある。

Rust verifier は candidate SHA、lockfile digest に加え、受理した workflow run の log / receipt へ
独立に固定した archive SHA-256 を期待値として受け取り、archive と sidecar の組を検査する。
download した sidecar に書かれた digest を、そのまま `--expected-archive-sha256` の信頼根にしては
ならない。entry の重複、absolute path、`..`、backslash separator、symlink、非 canonical tar header、
非 canonical JSON、digest 差し替え、binary の build binding と provenance の不一致を一つでも検出した
場合は展開前に拒否する。

```text
kio-eval release verify \
  --archive <kio-...tar.gz> \
  --checksums <kio-...checksums.json> \
  --expected-archive-sha256 <accepted run receipt の SHA-256> \
  --source-repo <clean exact-candidate checkout> \
  --expected-commit <full candidate SHA> \
  --expected-lock-sha256 <Cargo.lock SHA-256>
```

`kio-eval` は release engineering 用の dev-only tool であり、利用者向け archive には含めない。
通常の development build は明示的に `unbound` であり、packager は受理しない。RC build は
version、full commit SHA、Git tree、`Cargo.lock` / `rust-toolchain.toml` の SHA-256、Rust version、
target triple、`all-features`、`release` profile を binary へ束縛し、packager と verifier の双方が
一致を確認する。`--source-repo` を指定した verifier は commit、Git tree、workspace version、lockfile、
toolchain file の binding を clean checkout から独立に再導出する。Git authority は tracked tree / index
と exact commit であり、source tree / index に変更がある build・package・source verification は拒否する。

binary binding と provenance は schema v2 で、target によって次の closed な `repro_recipe` を必須にする。
古い binding/provenance schema や target と一致しない recipe は受理しない。

```text
x86_64-unknown-linux-gnu = linux-rustc-default-v1
aarch64-apple-darwin = macos-rust-lld-no-uuid-macos11-v1
x86_64-pc-windows-msvc = windows-msvc-brepro-v1
```

MSVC candidate build は `CARGO_ENCODED_RUSTFLAGS` を ASCII Unit Separator (`0x1f`) で区切り、
正確に `-C`、`link-arg=/Brepro` を渡す。これは rustc の stable な encoded rustflags 形式であり、
ambient Rust flags、compiler wrapper、target linker override、および locked graph の native C/asm
compiler・archiver・flags・dependency build override を継承せず、Cargo command-line config で `rustc`、
wrapper 無効、Windows の `link.exe` を固定する。bare tool 名は workflow が提供する trusted
Rust/C/MSVC toolchain の `PATH` から解決し、Windows SDK/MSVC探索用の `LIB` / `INCLUDE` と
registry/git cache は lock-bound な create-only Cargo home へ regular-file copy するが、ambient
`CARGO_HOME` の config/credentials は受理しない。candidate Cargo は filesystem root から absolute
manifest path で起動するため repository/ancestor `.cargo/config*` も探索せず、ambient `CARGO_*`
configuration environment も継承しない。一方、MSVCの `CL` / `_CL_` / `LINK` / `_LINK_` による
追加option注入は拒否する。Windows MSVC
artifact の packager と verifier は PE32+ を厳格に
parse し、`IMAGE_DEBUG_TYPE_REPRO` (kind 16) を必須にする。欠落・破損・非 PE32+ は warning や skip
ではなく fail-closed である。Linux は既定 rustc recipe、macOS は既存の pinned `rust-lld`、
`linker-flavor=ld64.lld`、`-no_uuid` と `MACOSX_DEPLOYMENT_TARGET=11.0` を維持する。

## 12.1 macOS / Linux の install と uninstall

verification 済み archive を新しい directory へ展開し、archive 内 binary だけを導入する。
build tree の `target/release` を直接 install してはならない。

```sh
tar -xzf kio-0.1.0-rc.3-<target>.tar.gz
./kio-0.1.0-rc.3-<target>/bin/kio --version
install -d "$HOME/.local/bin"
install -m 0755 ./kio-0.1.0-rc.3-<target>/bin/kio "$HOME/.local/bin/kio"
```

`$HOME/.local/bin` を `PATH` へ追加する操作は shell 設定の所有者が明示的に行う。system directory
への install や管理者権限は要求しない。uninstall は導入した binary だけを削除する。

```sh
rm "$HOME/.local/bin/kio"
```

uninstall は利用者の scope 内にある `.kio`、device registry、cache を自動削除しない。これらは
binary と別の user data であり、利用者の明示判断なしに消してはならない。

## 12.2 Windows experimental の install と uninstall

PowerShell で verification 済み archive を新しい directory へ展開し、user-local directory へ
`kio.exe` を copy する。PATH の永続変更は自動化しない。

```powershell
tar -xzf kio-0.1.0-rc.3-x86_64-pc-windows-msvc.tar.gz
New-Item -ItemType Directory -Force "$env:LOCALAPPDATA\Kio\bin" | Out-Null
Copy-Item ".\kio-0.1.0-rc.3-x86_64-pc-windows-msvc\bin\kio.exe" "$env:LOCALAPPDATA\Kio\bin\kio.exe"
& "$env:LOCALAPPDATA\Kio\bin\kio.exe" --version
```

Windows RC の導入後も、自動 child-scope mutation は要求しない。直接選択した scope で次の手動経路
だけを用いる。

```powershell
kio init <scope-path>
Set-Location <scope-path>
kio index --approve --offline
```

uninstall は `Remove-Item "$env:LOCALAPPDATA\Kio\bin\kio.exe"` で binary だけを削除する。
`.kio`、registry、cache は macOS / Linux と同様に残す。

## 12.3 signing status

P3 draft artifact は全 platform で publisher identity を証明する signature を持たない。macOS arm64
artifact には実行可能性のため `rust-lld` が生成する **ad-hoc signature** があるが、certificate、
Team ID、publisher identity はなく、**not notarized** である。これは linker output の一部であり、
別の `codesign --sign` 処理は実行しない。Windows artifact は **Authenticode unsigned**、Linux
artifact に detached signature はない。この違いを provenance の閉じた platform-specific signing
fields に記録する。credential、Apple notarization、Windows certificate、Sigstore 等の signing 処理は、
別の明示承認と鍵管理方針が確定するまで実行しない。checksum / provenance は、独立に保持した archive
SHA-256 と組み合わせて改ざん検出と同一性確認を提供するが、publisher identity の代替ではない。
