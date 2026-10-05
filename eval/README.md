# Kio 検索評価ハーネス (synthetic)

`docs/09-mvp-scope.md` §4.3 の **Recall 評価規約 (ゴールデンクエリ)** を実行するための
合成コーパス + 履歴シナリオ + ゴールデンクエリ + 評価ランナー。

- Push/PR 評価経路は **Rust-only**。残る Python は
  `eval/python-exceptions.toml` と exact-match する manual な
  Python-native ML/SDK/render adapter だけであり、この synthetic lane では実行しない。
- 決定論: 凍結済み `corpus-fixture.json` を Rust generator が materialize する。2 回実行で byte 同一。
- 通常評価は Rust の `kio-eval` と評価対象の `kio` を使う。
  `cargo build --release --locked --all-features` 済み前提。

## ファイル構成

| ファイル | 役割 |
| --- | --- |
| `corpus-fixture.json` | **生成物の凍結正本**。305 文書の bytes と manifest を保持し、Rust `kio-eval generate-corpus` が検証して materialize する |
| `history-plan.json` | version・Rust generator identity・corpus manifest digestを固定した JCS+LF の closed operation plan。Rust 実装はバンドルした digest と完全一致するplanだけを受理 |
| `kio-eval replay-history` | retained capabilityと隔離device上で、各 scope の `init → offline index → manual snapshot → edit/rename/delete` を決定論再現。48 commitとstrict log evidenceを検証後にmanifestをcreate-only公開 |
| `golden-queries.jsonl` | ゴールデンクエリ (M3-1 / M3-2 / M3-3 各 16+ 件)。**リポジトリ保持の正本** |
| `history-manifest.json` | `kio-eval replay-history/v1` のstrictな出力fixture。plan/corpus digestと、scope別step/message/commit数の実行証拠だけを保持。操作materialの正本は`history-plan.json`に一本化 |
| `golden-queries-crossscope.jsonl` | **横断増補 16 問** (09 §4.3、2026-07-26 凍結)。expected が必ず 2 scope に跨る。正解担体は既存 anchor そのもので、コーパスには手を入れていない |
| `kio-eval crossscope` | 横断増補のRust専用ランナー。full evaluatorのセット全体ゲートを部分集合へ誤適用せず、Recall/latency/Evidenceと診断値`worst_expected_rank`を検証する |
| `crossscope-results.json` | 現行の replica 単独経路で再生成した横断評価結果。per-query の `aggregator` や `aggregator_applied` は出力せず、`counts` に `worst_expected_rank_mean/max` を記録する |
| `crossscope-results-no-replica-2026-07-26.json` | 2026-07-26 の比較対照を保存した**履歴成果物**。移行前 schema の `aggregator_applied` を意図的に含むが、現行 runner の出力や入力には用いない |
| `kio-eval rerank dump --dataset synthetic` | current-tree queryの候補順・3要素key・検証済みChunk CAS textをcreate-only JSONへ固定するRust経路。offline GPU差分のpass 1 |
| `golden-queries-qhard.jsonl` | 実データの raster PDF / 図表・画像を正解担体にする、凍結済み Q_hard 8 問。digest も Rust runner が固定照合する |
| `kio-eval benchmark qhard` | attest済み外部fixtureとsynthetic M3-1を同一実行で再測定する唯一のQ_hard判定経路 |
| `golden-queries-fixture-b.jsonl` | baseline 比較専用の別凍結母集団（24問、hard1/2/3 各8、sha256:bdad3e02c4b70f721e882d7f24c8b5b442621be7c0c03593afde41b8ebca7d45） |
| `kio-eval benchmark baseline` | attest済みfixture-BをSpotlight/rgaと比較する唯一のbaseline判定経路 |
| `kio-eval scale generate` | Rust v3 の決定論的性能 fixture 正本。current-text と history-overlay を別の create-only destination として生成する |
| `kio-eval scale prepare` | 各 leaf scope を正式な `init → offline index` 経路へ通す。history lane は base index 後に全20 scopeの edit/rename/deleteを適用して再indexする |
| `kio-eval scale attest` | manifest・commit/tree/chunk CAS・SQLite/FTS/vector・registryをread-onlyで再計算し、current/historical-only/deleted/physicalを分離する |
| `kio-eval scale benchmark` | 2 laneのattestationを同時に束縛し、text/vector/hybrid/history/deletedをfallbackなしで測定する |

## 使い方

コーパス本体は再生成可能なため **リポジトリにコミットしない** (一時ディレクトリへ生成する)。

```bash
# 0. バイナリ (未ビルドなら)
cargo build --release --locked --all-features

# 1. 合成コーパス生成
target/release/kio-eval generate-corpus --out /tmp/kio-eval-corpus

# 2. Rust履歴シナリオ再現 (single-link binary copyを使い、manifestはcorpus直下へcreate-only公開)
install -m 0755 target/release/kio /tmp/kio-replay-kio
target/release/kio-eval replay-history \
  --corpus /tmp/kio-eval-corpus --bin /tmp/kio-replay-kio

# 3c. 横断増補 (別ファイル・Rust専用ランナー、09 §4.3)。既存 50 問とは独立に走る
target/release/kio-eval crossscope --corpus /tmp/kio-eval-corpus --bin target/release/kio --dry-run --out /tmp/crossscope-unused.json
target/release/kio-eval crossscope --corpus /tmp/kio-eval-corpus --bin target/release/kio --out /tmp/crossscope-results.json

# 3a. dry-run: Rust の正本 evaluator が golden-queries / manifest を検証する
target/release/kio-eval --dry-run --corpus /tmp/kio-eval-corpus

# 3b. 本評価: Rust の正本 evaluator が Recall@10 をシナリオ別に集計。
#     kio search 未実装の間は全クエリ NOT-IMPLEMENTED → exit 2 (未実装を green にしない)。
target/release/kio-eval --corpus /tmp/kio-eval-corpus --bin target/release/kio

# 3c. シナリオ絞り込み (複数指定可)。最終HEADのCIは3シナリオを個別に実行する。
target/release/kio-eval --scenario M3-1 --corpus /tmp/kio-eval-corpus --bin target/release/kio

# 4. offline rerankerへ渡すcurrent-tree候補を、検証済みChunk CASから固定する。
#    --outは既存fileを上書きしない。
target/release/kio-eval rerank dump --dataset synthetic \
  --corpus /tmp/kio-eval-corpus \
  --bin target/release/kio --out /tmp/rerank-input.json

# 5. GPU が返した JSON を固定dumpへ適用する。--report もcreate-only。
target/release/kio-eval rerank apply --input /tmp/rerank-input.json \
  --output /tmp/rerank-output.json --report /tmp/rerank-report.json
```

`replay-history` は、既に open した executable と HOME/XDG directory を pathname
再解決なしで child process へ渡せる Linux だけで実行する。macOS/Windows では
descriptor-exec 相当の安全な境界がないため、corpus を変更する前に structured error で
fail-closed にする。`--bin` はsingle-link regular executableに限定し、Cargoが
`target/release/kio`を`deps/`とhardlinkした環境では、`install -m 0755`で`/tmp`へ
single-link copyを作ってそのpathを渡す。pathname fallbackやhardlink例外は設けない。

`crossscope` と `rerank dump` の出力は measured corpus 外の既存directoryに置き、
既存fileを上書きしない。再測定は新しい出力名を使い、比較後に採用するartifactだけを
明示的に更新する。

### exit コード (docs/09 §4.3, 2026-07-03 J2 裁定)

| 状況 | exit | 扱い |
| --- | --- | --- |
| 全シナリオ (対象) が Recall@10 >= 0.8 | `0` | PASS |
| `KIO-E-*-NOT-IMPLEMENTED*` 系のクエリが 1 件以上 | `2` | 未実装。Recall 判定は無効 (green にしない) |
| Recall 未達 / 実行失敗 (非 0 かつ非 3 exit・不正レスポンス・解決不能) | `1` | FAIL。当該クエリは recall 0 として集計に残す |

exit `3` (部分成功) は stdout の JSON を採点対象にする (実装後の部分成功や実バグを未実装で握り潰さない)。

## Q_hard の外部 fixture 計測

Q_hard は raster PDF / PPTX 図表 / 画像を正解担体とするため、合成 corpus とは別の
**明示的に attest された外部 fixture** でのみ計測する。fixture がない、attestation が
ない、または tree / environment / golden digest / scope 一覧が一致しない場合、
`kio-eval benchmark qhard` は失敗する。旧形式の checked-in 結果は current consumer も
Rust producer も持たないため削除済みであり、入力にも Done 判定にも用いない。

```bash
target/release/kio-eval benchmark qhard \
  --fixture-root /private/tmp/kio-fixture-run \
  --tree qhard --env-name qhard \
  --attestation /private/tmp/kio-fixture-run/qhard-attestation.json \
  --bin target/release/kio --out /tmp/kio-qhard.json
```

attestation は strict JSON の `{schema_version:1, fixture_id:"kio-qhard-v1", tree,
env_name, golden_sha256, fixture_content_sha256, scopes}` である。`fixture_content_sha256` は
fixture root の regular files/directories を名前・型・content digest で順序付きに列挙して
hash した値で、root の `qhard-attestation.json` 自身は除外する（自己参照を避ける）。`scopes` は fixture root からの正規化相対 path
で、実際に見つかる `.kio` scope と完全一致しなければならない。探索は symlink を辿らず、
directory / scope 数に上限を設ける。検索 subprocess は fixture XDG state だけを使う。
`--online-query` 時だけ `GEMINI_API_KEY` / `MISTRAL_API_KEY` を名前指定で転送し、値は report
に出力しない。

`--synthetic-corpus <generated-corpus>` を添えると、同じ `kio-eval` 呼出し内で frozen
synthetic M3-1 18 問を再測定し、Q_hard 8 問と合算した 26 問 / 21 hit gate を report に
記録する。外部の結果 artifact は受け付けない。指定なしの Q_hard-only 実行は evidence-only
であり、acceptance success にはならない。

## fixture-B baseline 比較

fixture-B の24問は Q_hard 8問 + synthetic M3-1 18問の26/21 gateとは別の、Spotlight/rga
比較だけの凍結母集団である。まず `.kio` を除く indexed/pristine source が p01..p20 で等価な
ことを安全に束縛し、その attestation を測定に渡す。

```bash
target/release/kio-eval benchmark baseline-attest \
  --fixture-root /private/tmp/kio-fixture-run --baseline-corpus /path/to/pristine \
  --out /tmp/kio-fixture-b-attestation.json
target/release/kio-eval benchmark baseline \
  --fixture-root /private/tmp/kio-fixture-run --baseline-corpus /path/to/pristine \
  --attestation /tmp/kio-fixture-b-attestation.json --bin target/release/kio \
  --comparator-runtime /Library/KioComparatorRuntime/v1 --out /tmp/kio-baseline.json
```

Rust 以外のbaseline runnerは保持しない。歴史的 JSON は証拠ではなく、新たな合格計測を成立させない。
`mdfind` または `rga` が無ければ `blocked-unmeasured` であり、pass にはならない。
baseline report の `configuration` は `online_query` と、Kio subprocess に実際に転送した
credential の**環境変数名だけ**を `forwarded_credential_names` として記録する。値は report に
書き出さない。`--online-query` を指定しない限りこの配列は空である。比較器欠落などで Kio lane
の起動前に block された場合は credential を転送していないため配列は空のままとし、
`online_query` だけで online mode の要求を監査可能にする。

### macOS 比較器 runtime の管理者構築

正式 baseline 用 runtime は既存の Homebrew tree を変更せず、専用 read-only image として一度だけ構築する。
管理者は、監査済みの canonical executable を `/usr/local/bin/kio-eval` に root-owned、group/other
非 writable、extended ACL なしで配置してから、直接この唯一の管理者コマンドを実行する。旧 `prepare` /
`finalize` command や shell wrapper は存在しない。

```bash
sudo /usr/local/bin/kio-eval benchmark comparator-runtime install
```

Rust が reviewed pin の nofollow copy、Mach-O closure/rewrite、payload re-walk、固定 config、mount admission、
DMG/manifest ACL・xattr policy、digest と create-only manifest publication を fail-closed で行う。DMG は
`FinderInfo`/`provenance` だけを許容し、attach が生成する `com.apple.diskimages.recentcksum` だけを削除してから
hash へ束縛する。runtime/manifest は `provenance` 以外の xattr と extended ACL を拒否する。

対象の `/Library/KioComparatorRuntime/v1`、image、manifest、一時 build root のいずれかが既に存在すれば停止する。
成功後に廃止する場合も、detach、image/manifest の削除、空ディレクトリの `rmdir` に限定する。

比較器プロセスは retained descriptor に束縛した pristine persona directory を CWD とし、相対 `.` を
入力に使うため、可変な public corpus path を再オープンしない。`kio` は検査済み regular executable
の private snapshot に束縛する。`rga` lane は、ユーザー所有の Homebrew tree を比較器 runtime として
受け付けない。代わりに `--comparator-runtime` で、管理者が提供した専用の **canonical absolute** runtime root を
明示する。root とその path component、runtime 内の file/directory、runtime 内 symlink の最終 target はすべて
root 所有かつ group/other 非書込みで、symlink target は root の外へ出てはならない。root には少なくとも次を
含める。

```text
bin/rga
bin/rga-preproc
bin/pandoc
bin/pdftotext
bin/rg
config/rga-config.json
```

macOS では evaluator が固定された sealed system `otool` を使って、この5 executable の Mach-O load command
closure を再帰的に解決する。`@loader_path`、`@executable_path`、`@rpath`、runtime 内 symlink を解決した後、
すべての runtime image が当該 root 内に残ることを要求する。terminal として許す root 外 dependency は、
root 所有・非書込みで再確認した `/usr/lib` または `/System/Library` 下の macOS sealed-system library、
または Apple-signed `dyld_info` の strict catalog で UUID と全依存 edge を束縛した dyld shared-cache image
だけである。catalog の形式不整合、途中切断、未解決 edge は fail-closed にする。ただし catalog と sealed
filesystem の双方に存在しない edge は、dyld が明示する `weak-link` に限って不在を許可し、親 image の
edge に加えて `missing_weak_dylibs` として closure provenance へ記録する。通常 link、`delay-init` 単独、
未知 attribute、または NotFound 以外の検査失敗は引き続き fail-closed である。不在 weak image が後から
catalog または filesystem に現れた場合は closure digest が変化し、計測を block する。
実行ファイルの dynamic loader は sealed `/usr/lib/dyld` のみ、`LC_DYLD_ENVIRONMENT` は禁止する。
未解決の `@rpath`、runtime 外への escape、非sealed component、closure の変更は fail-closed にする。
report は runtime root、固定 inspector、各 closure image の canonical path・trust class・SHA-256 と closure digest を
記録する。従って runtime verification failure や comparator 欠落は `blocked-unmeasured` であり、pass にはならない。
各 rga subprocess の実行前後と計測 finalization では load command から closure を再帰的に再解決し、初期 binding の
canonical path・trust class・SHA-256・closure digest と完全一致させる。途中で高優先度の `@rpath` 候補が追加された場合を
含め、entry の追加・削除、解決先または内容の変化は `blocked-unmeasured` とする。
runtime root は macOS `MNT_RDONLY` の read-only mount でなければならない。bind時、各 rga subprocess の直前・直後、
計測 finalization で public canonical path と retained root descriptor の mount identity を再確認し、writable filesystem、
unmount/remount、mount replacement、または確認不能は `blocked-unmeasured` とする。report には canonical runtime path、
read-only 判定、filesystem ID・mount point・mount source・filesystem type・flags、および closure digest を保存する。
`config/rga-config.json` は `{"custom_adapters":[]}` だけを含む root-owned sealed regular file とし、rga の
ユーザー設定・任意 custom adapter を取り込ませない。helper lookup の `PATH` も private temporary directory ではなく
sealed runtime の `bin/` だけに固定する。

macOS の `mdfind` はコピー実行を許さない system tool のため、例外として
正確な `/usr/bin/mdfind` のみを直接実行する。その場合も capability preflight、実体 digest の束縛、および
各 query 後の再照合を必須にする。比較器の欠落・preflight failure・予期しない `mdfind` failure、または
`rga` の 0/1 以外の終了は、miss として margin を有利にせず測定を block する。

## シナリオと評価コーパスの対応 (docs/09 §4)

| シナリオ | フラグ | コーパス上の対象 | 判定 |
| --- | --- | --- | --- |
| **M3-1** 現行検索 | (なし) | 編集/リネーム/削除しない安定 anchor。本文の数値・用語の部分記憶で引く | 現行 tree の該当 file がヒット |
| **M3-2** リネーム追跡 | `--all-history` | リネームされた anchor (旧名で記憶) + 編集された anchor (旧値は履歴のみ) | 旧 raw_hash の chunk が両方ヒット |
| **M3-3** 削除再発見 | `--include-deleted` | 削除された anchor の数値を再発見 | 削除済み file の chunk がヒット |

`expected` は `{scope, file, section}` の分離形式 (docs/09 §4.3、`03-data-model.md` §3
「直下のみ」規則)。`section` は **英語ニーモニック** (例 `"recall"`) であり、実 `section_id` ではない。

### expected → (raw_hash, section_id) の解決 (J2 裁定, 2026-07-03)

Recall の突き合わせ単位は `(raw_hash, section_id)` (docs/09 §4.3)。`expected` はニーモニックと
`{scope, file}` の分離形式なので、ハーネスが以下の手順で実値へ解決する:

1. **section (ニーモニック) → heading (実見出しテキスト)**
   現行内容は`corpus-manifest.json`、履歴操作対象は凍結`history-plan.json`の
   `sections[]`が`{slug: ニーモニック, heading: 実見出し}`を持つ。
   `history-manifest.json`は操作materialを重複保持せず、実行証拠だけを記録する。
   例: `"recall"` → 見出し `"回収率と精度"`。
2. **heading → section_id** … `docs/04-pipeline.md §4.1` の slug 規則で `slugify(heading)`。
   例: `slugify("回収率と精度")` = `"回収率と精度"` (日本語は保持。英語ニーモニックとは一致しない)。
   これが J2 の核心: `"recall"` を実 `section_id` として突き合わせると必ずミスする。
3. **{scope, file} → raw_hash** … 現行内容は`corpus-manifest.json`、M3-2
   (編集/リネーム) / M3-3 (削除) の旧内容は`history-plan.json`が記録する
   `before_raw_sha256`から`raw_hash = "sha256:" + digest`を得る。

突き合わせ時、`evidence_pointer.section_id` (docs/08 §2 では heading_path を "/" 連結した slug) は
最深 (leaf) セグメントを取り、`slugify(heading)` と比較する (見出し「回収率と精度」→ `"回収率と精度"`)。

### Recall 実測の gate タイミング

- Step 3 当時の gate は M3-1 のみだった。`--all-history` / `--include-deleted` が揃った現在は、
  **最終HEADのCIで M3-1 / M3-2 / M3-3 をすべて個別実行**する。

## dogfood との関係 (docs/09 §4.3)

評価コーパスは 2 種:

- **synthetic** (本ハーネス): 公開可能な生成文書。CI / Done 判定の**正本**。数値を公開してよい。
- **dogfood**: 開発者自身の実フォルダ (非公開)。数値は公開せず、3 シナリオの**主観成功確認**に使う。
  本ハーネスは dogfood の数値を扱わない。dogfood は同じ `golden-queries` 形式を各自ローカルで用意する。

Done 条件 = **synthetic で各シナリオ Recall@10 >= 0.8** + **dogfood で 3 シナリオの手動成功確認**。

## クエリ凍結規律 (重要)

`docs/09-mvp-scope.md` §4.2 に従う。既存クエリの差し替え・削除は禁止し、悪化を隠すための削除もしない。
クエリを増やす場合は、結果を測定する前に別ファイルとして追加し、下表へ件数と digest を記録して再凍結する。
凍結 corpus fixture (`corpus-fixture.json`) と履歴シナリオも凍結対象であり、Recall 数値の連続性を壊す変更をしない。

| ファイル | 件数 | digest (raw UTF-8 bytes の sha256) |
| --- | ---: | --- |
| `golden-queries.jsonl` | 50 (M3-1: 18 / M3-2: 16 / M3-3: 16) | `sha256:b7183fa3586383883ec522256696268eab8e607c1a032020e09223158a5bf08d` |
| `golden-queries-qhard.jsonl` | 8 | `sha256:d5c30eccc664e6bd4d96e1068970e225d209d04bde34c50eab300d6245d4e163` |
| `golden-queries-crossscope.jsonl` | 16 (M3-1: 8 / M3-2: 4 / M3-3: 4) | `sha256:1fe0ebf2b51f35323d91bb1a235a282b5fa68a59de7a9c0bac2bc0f4ebade868` |
| `golden-queries-fixture-b.jsonl` | 24 | `sha256:bdad3e02c4b70f721e882d7f24c8b5b442621be7c0c03593afde41b8ebca7d45` |

横断増補は、Recall@10 では横断融合の順位欠陥をほぼ検出できないため、診断値 `worst_expected_rank`
(2 つの expected の遅い方の順位) を併記する。

## 決定論の検証

```bash
target/release/kio-eval generate-corpus --out /tmp/c1
target/release/kio-eval generate-corpus --out /tmp/c2
diff -r /tmp/c1 /tmp/c2   # 差分なし (byte 同一) であること
```

`history-manifest.json` の plan/corpus digestとscope別step/message/commit数も、別のフレッシュ
コーパスに Rust replay すれば byte 同一になる。edit/rename/delete materialは凍結
`history-plan.json`だけが保持し、manifestへ重複させない。commit hash / timestamp は非決定なので
manifestに含めない。同じcorpusでの2回目実行は、半端な履歴へ追記せずfail-closedに拒否する。

## 20 scope / 12 万 chunk 性能 fixture

§4.1 の「20 scopes / 合計 10 万 chunk」性能ゲートは、Recall の凍結済み 305 files / 7 scope
corpus を水増しせず、独立した fixture で測る。最初の層は再現性を優先した
**balanced current-text fixture** であり、`full` の形を次で固定する。

- 20 leaf scopes × 200 Markdown files × 30 ATX sections = **120,000 current chunks**
- 14 の利用者属性、20 の用途を engineering / research / ML-data / product / security / client / inbox に分ける
- 1 section を既定 `[chunking] strategy="heading", max_chars=6000` の 1 chunk 未満に保つ
- Kio は scope 直下だけを対象にするため、collection root 自体は scope にせず、20 leaf folders を個別 scope にする

folder と利用者・用途の対応は manifest にも保存する。

| folder | 主な利用者属性 | ユースケース |
| --- | --- | --- |
| `engineering-architecture` | software engineer | architecture / ADR |
| `engineering-api-specs` | software engineer | API contracts |
| `engineering-incidents` | SRE | incident response |
| `engineering-runbooks` | SRE | operations runbooks |
| `engineering-releases` | release engineer | release / migration notes |
| `research-papers` | academic researcher | paper library |
| `research-lab-notes` | academic researcher | laboratory notebook |
| `research-experiments` | academic researcher | experiment results |
| `research-grants` | principal investigator | grants / budgets |
| `research-literature` | graduate student | literature notes |
| `ml-model-evaluations` | ML engineer | model evaluation |
| `data-dictionaries` | data engineer | data dictionary / lineage |
| `data-dashboard-reports` | data analyst | dashboard reports |
| `ml-notebook-exports` | ML engineer | notebook exports |
| `product-meetings` | product manager | meeting decisions |
| `product-requirements` | product manager | requirements / user research |
| `product-roadmaps` | engineering manager | roadmap / capacity planning |
| `security-compliance` | security engineer | controls / audit / threats |
| `client-deliverables` | consultant | findings / recommendations |
| `downloads-inbox` | knowledge worker | downloaded references / inbox |

full は時間・ディスクを使う明示実行専用であり、push CIでは生成・indexしない。通常の安全確認には
同じ20 scope構造の `tiny`（各scope 3 files、current-text 180 chunks）を使う。2 laneは同じpathへ
上書き・adoptせず、必ず別destinationへ生成する。

```bash
# 軽量 smoke (20 scopes / current-text 180 chunks)
target/release/kio-eval scale generate \
  --out /tmp/kio-scale-tiny-current --profile tiny --lane current-text
target/release/kio-eval scale generate \
  --out /tmp/kio-scale-tiny-history --profile tiny --lane history-overlay
target/release/kio-eval scale prepare \
  --corpus /tmp/kio-scale-tiny-current --bin target/release/kio
target/release/kio-eval scale prepare \
  --corpus /tmp/kio-scale-tiny-history --bin target/release/kio
target/release/kio-eval scale attest --corpus /tmp/kio-scale-tiny-current
target/release/kio-eval scale attest --corpus /tmp/kio-scale-tiny-history
target/release/kio-eval scale benchmark \
  --current-corpus /tmp/kio-scale-tiny-current \
  --history-corpus /tmp/kio-scale-tiny-history --bin target/release/kio \
  --warmups 1 --samples 1 --out /tmp/kio-scale-tiny.benchmark.json

# 本番規模 (20 scopes / base 4,000 files / 120,000 chunks): P4手動計測時のみ
target/release/kio-eval scale generate \
  --out /tmp/kio-scale-full-current --profile full --lane current-text
target/release/kio-eval scale generate \
  --out /tmp/kio-scale-full-history --profile full --lane history-overlay
target/release/kio-eval scale prepare \
  --corpus /tmp/kio-scale-full-current --bin target/release/kio
target/release/kio-eval scale prepare \
  --corpus /tmp/kio-scale-full-history --bin target/release/kio

# 任意時点で再検証 (read-only SQLite attestation)
target/release/kio-eval scale attest --corpus /tmp/kio-scale-full-current
target/release/kio-eval scale attest --corpus /tmp/kio-scale-full-history

# Rust measurement lane: Full の formal sampling shape は 5 warmup + 100 samples。
# P2 は bounded smoke までとし、formal測定、実データのD1判定、dogfood合格はP4の別手動gateである。
# --out は corpus 外の既存実体 directory にだけ原子的に書き出せる。
target/release/kio-eval scale benchmark \
  --current-corpus /tmp/kio-scale-full-current \
  --history-corpus /tmp/kio-scale-full-history --bin target/release/kio \
  --warmups 5 --samples 100 --out /tmp/kio-scale-full.latency.json
```

`kio-eval scale prepare` は各scopeを明示的な `kio index --offline --yes` で終える。`index` 自体がsnapshotと
HEAD tree projectionを公開するため、その直後に別の`kio snapshot create`を追加しない。history laneはbase
HEADを作った後、凍結planどおり各scopeへedit・rename・deleteを1件ずつ適用し、同じCLI index経路でfinal
HEADを作る。SQLite/CASへ直接データを注入しない。device stateはcorpus内の`.kio-eval-device`に隔離される。
prepare/search childだけがrelease buildでも有効なexact selector
`KIO_EVAL_DETERMINISTIC_EMBED=scale-v3`を受け取り、実`EmbeddingAdapter` wireで768次元の決定論vectorを作る。
このadapterはnetwork不可・非課金であり、開発者のregistry、API key、外部modelを使わない。
current fixtureのvector/hybrid workloadでは、ASCII token featureに加え、各chunkで最初に現れる12桁hexの
reference tokenだけをdomain-separated anchorとして扱う。query自体も同じopaque tokenであり、後続referenceを
すべてanchor化して長いchunk内で信号を希釈しない。このalgorithmはembedding profileのimmutable model versionへ
束縛され、attestorはadapterを呼ばずに同じvector bytesを独立再計算する。これはscale-v3 exact-reference workloadの
決定論的な検索probeであり、一般的なsemantic embedding品質や将来のhistory-vector laneを主張しない。

出力manifestはbase/overlay source hash、operation plan、expected populationを固定し、
prepare reportはscopeごとのbase/final commit・treeを束縛する。`scale-attestation.json`は次を証明する。

- lane/profile/manifestとsource bytesの完全一致、isolated registryのindexed 20 scopes完全一致
- 生CASからbase→final commit/tree関係、凍結adapter setのtool-lock preimage、chunk canonical bytes/hash、
  publication/index projectionを再計算
- current、historical-only、deleted、physical CAS、embedding/vector rowを別々に数え、凍結母集団と完全一致
- history laneの`edit_operations` / `rename_operations` / `delete_operations`が各20件であり、
  current-text laneでは各0件、かつ両laneの母集団とdestinationが独立していること
- full current-textは120,000 current chunks、full historyは119,400 current、1,200 historical-only、
  600 deleted、120,600 physical chunksであること
- 各scopeの検索標本は期待section内に1回だけ現れる決定論reference tokenを使い、共通語によるscope順位tieを避ける。
  これは高選択性queryのlatency probeであり、広いqueryのmulti-scope ranking性能は証明しない

Rustの`kio-eval scale benchmark`はrelease binary、両manifest、両prepare/attestation、実測前後のlive evidence、
platformを1 reportへ束縛する。5 laneは`current-text`、`vector`、`hybrid`、`history`、`deleted`で、modeは
すべて明示する。`requested_mode == resolved_mode`、`fallback == false`を必須とし、vector/hybridのtext fallbackは
測定値ではなく失敗である。各top-10 Evidence Pointerはproduction issuer/verifierを使わないevaluator-local wireと
生CASから再検証する。reportはlaneごとの母集団、Recall@10、生標本、product metricとprocess wallの
p50/p95/p99、binary/fixture/attestation digest、独立検証したPointer件数を保持する。deleted laneは
正解Pointerをprivateなcreate-only出力先へ実際に`restore --to`し、復元raw hashの一致とfixture working
treeの不変をそれぞれ`restore_raw_verified` / `restore_working_tree_unchanged`として`true`に固定する。

D1 schemaもRust reportに含めるが、このP2 scale runはD1 corpusのTTFV/costを測っていない。baseline/enriched
TTFV、preview/actual costはそれぞれtyped `not-measured`として出力し、0またはpassへ変換しない。Fullの
5 warmup/100 samplesはP4 manual gateであり、既存M3-1 current-text p95 < 5秒とM3-2/M3-3
history/deleted p95 < 7秒をproduct/process-wallの両signalで判定する。vector/hybridには未合意の閾値を
追加せず実測値を保持する。D1実データとdogfoodもP4の別gateであり、push CIのTiny smokeは正式性能合格にならない。

### このfixtureで証明しないもの

- 全scopeが6,000 chunks、全ファイルが同じ生成Markdownであり、実フォルダの偏ったscope規模、
  日本語、表、ログ、コード、長短文書の混在は代表しない。
- exact reference tokenを使うため、広い共通語queryで20 scopeの候補が競合するranking/latencyは証明しない。
- D1の実測、外部OCR、paid embedding、GPU、real dogfoodの品質・性能は証明しない。
- Q_hardのSpotlight/rga比較、dogfood、D1 (PDF 1,000本/5GB相当、TTFV/AI時間/コスト) は
  性質の異なるゲートなので、このbalanced fixtureへ混ぜない。

次の拡張順は (1) scope/chunk長を偏らせたskewed robustness fixture、
(2) Q_hard/D1/dogfood の実データ系ゲートとする。balanced fixtureの再現性は維持する。

生成先は owner marker で保護する。非空の未所有 directory は変更せず、`--reset-owned` も未知の
entry があれば削除前に停止する。ready corpus の再生成と再 prepare は no-op になる。

```bash
cargo test -p kio-eval --all-targets --locked
```

## 20人の独立 persona-PC fixture（Rust-only contract）

persona-PC suite の唯一の semantic authority は Rust の canonical artifacts である。
`plan` は persona、Rust scope ID、home path、allocation、source identity を固定し、
`schedule` と `render` は同一 plan に束縛された deterministic projection である。Python
に semantic parser、generator、renderer、materializer、scaffold、prepare/replay runner はない。

通常の Rust CI は Tiny/Pilot の凍結ベクタに加え、唯一の Full plan/source-projection authority で
195,000 source / 2,400,000 chunk の契約を検証する。重複する Full suite schedule、render artifact、
consumer、scaffold stress は通常 CI に含めない。Full source projection、suite schedule、render artifact
をそれぞれ一度だけ生成して凍結 digest と上限を検証する明示的な Rust-owned manual command は次である
（Rust 1.98.0）。

```sh
cargo +1.98.0 run --release --locked -p kio-eval --example persona_full_contract
```

この command は deterministic JSON summary を出力する。これは cold/full fixture generation の固有信号であり、
通常 CI の高速な contract lane と混同しない。example は通常 CI で compile されるが、実行はしない。
完全なcold build、二つの独立process/environmentでの一致比較、Full scale、U7、OCRの唯一の
manual入口は [manual full/cold gate](../tasks/manual-full-cold-gates.md) に集約する。

```bash
target/release/kio-eval persona plan --profile tiny \
  --out /private/tmp/kio-persona-tiny-plan.json
target/release/kio-eval persona schedule \
  --plan /private/tmp/kio-persona-tiny-plan.json \
  --out /private/tmp/kio-persona-tiny-schedule.json
target/release/kio-eval persona render \
  --plan /private/tmp/kio-persona-tiny-plan.json \
  --out /private/tmp/kio-persona-tiny-render.json
target/release/kio-eval persona materialize \
  --plan /private/tmp/kio-persona-tiny-plan.json \
  --schedule /private/tmp/kio-persona-tiny-schedule.json \
  --render /private/tmp/kio-persona-tiny-render.json \
  --destination /private/tmp/kio-persona-artifacts --replay-id replay-01
target/release/kio-eval persona scaffold \
  --plan /private/tmp/kio-persona-tiny-plan.json \
  --root /private/tmp/kio-persona-workspaces
```

Both Rust publication commands are create-only. Materialization publishes only
the three canonical artifacts and `persona-materialization.json`; scaffold
publishes workspace topology and `persona-workspace-owner.json`. A plan profile
is planning/materialization evidence only: it is not Kio prepare/index/replay,
chunk, history-readiness, or performance evidence.

Writer coordination and filesystem observation are Rust-only. Acquire the
parent lease, acquire and release each plan-owned scope lease, release the
parent lease, then publish a create-only attestation. Scope calls use the Rust
scope ID, never a home path; the CLI derives all record bindings itself and
accepts no caller-supplied owner or materialization digest:

```bash
kio-eval persona lease scope claim \
  --root /private/tmp/kio-persona-workspaces --persona p01 \
  --scope-id <rust-scope-id> \
  --parent-session parent-01 --worker-session worker-01
kio-eval persona attest \
  --root /private/tmp/kio-persona-artifacts \
  --out /private/tmp/kio-persona-attestation.json
```

`persona attest` is a bounded Rust filesystem observation of the exact
materialized artifacts. It explicitly reports no actual Kio evidence and no
history readiness; it neither establishes replay nor search success.

Push/PR CI は Python を起動しない。tracked Python の閉包は Rust の例外台帳 test が
検証し、manual adapter 自身はネットワーク・GPU・課金なしの通常 CI から除外する。
