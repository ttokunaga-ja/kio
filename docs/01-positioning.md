# 01 Positioning

Kio のプロダクト位置づけ・対象ユーザー・差別化・競合分析・MVP スコープ・Phase plan を **正本** として定義する。他ドキュメントが「Kio とは何か」を語る場合、本書を参照する。

> 関連: [02-philosophy.md](02-philosophy.md) (理念) / [09-mvp-scope.md](09-mvp-scope.md) (MVP / Phase / Step) / [10-operations.md](10-operations.md) (横断規約)

---

# 1. ポジショニング (一行で)

```
英: Local-first knowledge archive, powered by frontier AI.
日: データはローカル、計算は最強の AI を使う。
```

二次表現 (補助的な言い換え):

```
英: Evidence-grounded local knowledge archive
日: 原文根拠付きローカル知識アーカイブ
```

Kio は次のいずれでもない:

- 全部入りの "Git for knowledge"
- 個人向け AI 検索ツール (Khoj / AnythingLLM 系)
- OS 級プロダクト
- 企業向けナレッジ基盤
- Knowledge Graph プラットフォーム
- offline-first 原理主義ツール (everything offline)

Kio は次である:

> **ローカルファイルを、過去も含めて、AI と人間が根拠付きで探索できる知識アーカイブ。データの主権はローカルに置きつつ、計算は frontier AI (Mistral OCR / Gemini / Claude / GPT) を含む最強の手段を使う。**

## 1.1 なぜ "local-first" であって "offline-first" ではないか

`local-first` は **データの主権がローカルにある** ことを意味する。`offline-first` は **ネット遮断でも動く** ことを含意する。両者は別物である。

Kio の対象ユーザー (開発者・研究者) の現実のワークフローは、Markdownize や Embedding に Mistral OCR / Gemini / Claude / GPT 等の frontier AI を使うのが既定値である。ここで "everything offline" を強要すると、Perkeep が辿った「思想は近いが日常体験差を出せない」失敗を踏襲する。

Kio の主張は「**原本・履歴・index の保管と主権はあなたのマシンにある / 計算結果 (Markdown, Embedding) もあなたのマシンに残る / 処理のためのファイル内容の送信は明示 opt-in で行い、何を・いつ・どの Adapter へ送ったかを記録する** ([07-adapter-spec.md §3](07-adapter-spec.md))」であり、API 呼び出し自体を禁止することではない。opt-in 後は frontier AI にファイル内容が送信される。これを隠さず、preview で network transmission policy として提示する ([06-cli-spec.md §2](06-cli-spec.md))。完全オフライン運用はローカル LLM Adapter や同梱 deterministic Adapter を選択するユーザーの自由として残るが、それは **デフォルトではない**。

opt-in の単位・寿命・revoke の正本は [07-adapter-spec.md §3](07-adapter-spec.md)。

---

# 2. ターゲットユーザー (最初の顧客)

最初のターゲットは明確に絞る。

```text
- 大量の PDF・Markdown・コード・画像・研究資料を扱う
- 開発者・研究者・技術者
- Git や CLI に抵抗がない
- ローカルファイルが散らかっている
- AI 検索を試したいが、クラウド丸投げは嫌
```

**MVP では一般ユーザー向けではない**。現在の操作面は CLI + 構造化出力 `--json` だけである。

---

# 3. 第一価値命題

最初に売るのは思想ではなく即効的体験差。

> **「探せなかったファイルがすぐ見つかる」**
> **「根拠が死なない」**

最低ライン:

```bash
kio init
kio index --approve      # 取り込み + ベースライン index (初回は preview + 明示承認)
kio search "あの PDF"
kio open <検索結果の pointer>
```

これで価値が成立する状態を MVP の Definition of Done に含める。`kio index` が**初回の**取り込みと検索 index 構築の入口であり (後着の online 成果は batch resume / retry / reindex の finalize が検索対象化する — [05-runtime.md §8.1](05-runtime.md))、成功時に auto snapshot も作られる — 明示 `kio snapshot` は任意。この 4 コマンドは **API キー未設定でも成立する** (同梱 deterministic Adapter によるベースライン index + text 検索。[07-adapter-spec.md §2.1](07-adapter-spec.md))。既定 local の EG2 はモデル準備後に意味検索・画像検索を強化する。初回モデル取得前の baseline は FTS のままである。取得契約は [10-operations.md §5.1](10-operations.md)、Adapter は [07-adapter-spec.md §5.3](07-adapter-spec.md) を参照する。frontier AI は OCR 等を強化する opt-in である。

即効価値と履歴価値は分けて訴求する。

- **初日から効く**: 意味検索 (語彙一致しない言い換え・スキャン PDF・画像内テキストに届く) と Evidence Pointer 表示 (意味検索は EG2 モデル準備済み、または HTTP Adapter 承認済み構成の Done 条件。モデル未準備・HTTP 未承認時は text 検索ベースライン)。OS 全文検索 (Spotlight / ripgrep-all) が失敗するクエリで勝つことを Done 条件に含める ([09-mvp-scope.md §4](09-mvp-scope.md))。
- **使うほど効く**: リネーム横断 (M3-2)・削除済み再発見 (M3-3) は snapshot 履歴の蓄積とともに立ち上がる価値であり、導入初日の差別化としては訴求しない。

---

# 4. 差別化の核 (3 点に絞る)

### 4.1 Evidence Pointer

`commit / tree / raw_hash / chunk_hash / span` で根拠を指す。path ではない。ファイル移動・削除・上書きでも根拠は死なない (CAS object store が原文を保持するため。**明示の purge を除く** — [02-philosophy.md §2.4](02-philosophy.md))。

### 4.2 Markdown 正規化を共通中間表現とする

すべてのファイル種別を Normalized Markdown に変換し、人間と AI が同じビューを使う。**Markdown は正本ではなく raw object からの read-only view**。詳細は [03-data-model.md §10](03-data-model.md)。

### 4.3 Content-addressed local archive + time-travel search

全ファイルを CAS object として保存し、snapshot DAG を持つ。削除済み / 過去版 / 移動済みファイルにも検索が届く。

---

# 4.4 競合・近接事例

「自分だけが新しい」という前提で進めると埋もれる。比較は思想や機能数ではなく、**現在のユーザー体験差**と、将来クラウドへ拡張する場合の**戦略上の脅威**を分けて行う。

> **調査基準日: 2026-08-23。** 以下は各社の公式公開資料で確認できた範囲に限る。未確認の機能を「存在しない」と断定せず、性能・復旧時間・顧客数等のベンダー主張を独立検証済みの値として扱わない。Kio Cloudに関する記述はすべて**未実装・未承認の将来仮説**であり、現行MVPの機能ではない。

競合は一列に並べず、次の四層で追う。

```text
Layer A: Local knowledge access
  DocsAgent / Constella / Msty / Obsidian ecosystem / Khoj / AnythingLLM

Layer B: AI-native sharing and current-state enterprise search
  Box Hubs / SharePoint Agents / Dropbox Dash / Glean / Notion / Gemini Notebook

Layer C: Secure content collaboration + recovery
  Egnyte / Box / Microsoft / Dropbox

Layer D: Versioned file data + cyber recovery baseline
  Nasuni / Rubrik / Cohesity / Veeam
```

## 4.4.1 現行Local方針・MVPの直接競合と第一代替

| プロダクト | レイヤー | Kio との重なり | Kio が維持すべき体験差 |
| --- | --- | --- | --- |
| [**DocsAgent**](https://github.com/docsagent/docsagent) | local document intelligence + MCP | ローカル文書のparse/index、CLI検索、semantic search、Agent接続 | 現在版を検索できるだけでなく、改名・移動・削除・上書き前を含む履歴検索と、不変Evidence Pointerを一続きで示す |
| [**Constella**](https://github.com/Constella-OS/constella-desktop) | local knowledge substrate + graph + MCP | folder index、SQLite/LanceDB、knowledge graph、local/cloud model、MCP | personal knowledge graphではなく、raw原本・snapshot・検証可能な履歴を持つarchiveに集中する |
| [**Msty Knowledge Stacks**](https://docs.msty.app/features/knowledge-stack/basics) | device-local RAG workspace | file/folder/Obsidian取り込み、端末内処理、content Q&A | RAG corpusではなく、原本identity・過去版・削除済み資料まで辿れるversioned archiveとする |
| [**Perkeep**](https://perkeep.org/) | content-addressed personal storage | content-addressed・ローカル中心・思想 | Markdown正規化、AI検索、Evidence Pointerと、初日から効く検索体験を提供する |
| [**git-annex**](https://git-annex.branchable.com/) | 大容量ファイル × Git | content-addressedの発想、CLI中心 | 大容量同期ではなく、異種文書の知識検索・正規化・原文根拠へ集中する |
| [**Obsidian + Smart Connections**](https://smartconnections.app/smart-connections/) | note vault + local semantic search | local-first AI検索、ローカルembedding | vaultを置き換えず、任意のindexed scopeとファイル形式・履歴を横断する |
| [**Khoj**](https://docs.khoj.dev/) / [**AnythingLLM**](https://anythingllm.com/) | personal AI / local-first AI app | 文書取り込み、検索、chat | chat UXでは競わず、content-addressed archive、time-travel、Evidence PointerをAPI・CLIへ出す |
| [**DEVONthink**](https://www.devontechnologies.com/apps/devonthink/ai) | 文書管理アプリ | ローカル中心、文書管理、検索 | 文書管理UIではなく、既存folderを置き換えない履歴・証拠layerに徹する |
| [**Spotlight**](https://support.apple.com/guide/mac-help/search-with-spotlight-mchlp1008/mac) / [**Recoll**](https://www.recoll.org/) / [**ripgrep-all**](https://github.com/phiresky/ripgrep-all) | OS・ローカル全文検索 | ローカル、即時、低コスト。「本文の一部から探す」第一代替 | 言い換え・scan/画像、削除・上書き済みの過去版へ届き、path変更後も根拠を解決する |
| [**Gemini Notebook**](https://support.google.com/gemininotebook/answer/16322204?hl=en) | cloud source-grounded notebook sharing | source集合へのQ&A、出典、共有 | local truth、任意folder横断、snapshot履歴、不変Evidence Pointer、ローカル原本回帰を結ぶ（詳細 §4.4.4） |
| [**Zotero**](https://www.zotero.org/) / [**Paperless-ngx**](https://docs.paperless-ngx.com/) | 専用文書archive | PDF/OCR、metadata、ローカル運用 | 専用libraryを置き換えず、その外側も含めて横断し、CAS履歴とEvidenceを加える |
| [**Pieces**](https://pieces.app/enterprise) | personal/team work memory | 時系列context、AIからのrecall | 人の活動memoryではなく、組織資料の版・raw原本・claimの根拠を正本化する |

Local MVPの競合比較では、「ローカルファイルをindexする」「PDFをembeddingする」「Agentから検索する（競合のMCP / Kioの現行CLI・構造化JSON）」だけでは差別化にならない。Kioの比較デモは、同じ検索結果を出すことではなく、**過去版・削除済み資料を発見し、どのraw source identityに由来し、Normalized MarkdownのどのUTF-8 spanを根拠とするかを後日も検証できること**まで含める。MCP / external Agent APIは未承認roadmapであり、現行MVPの提供機能として扱わない（[09-mvp-scope.md §2](09-mvp-scope.md)）。

## 4.4.2 将来Cloudの戦略競合

| 競合 | 公式資料で確認できる重なり | 脅威 | Kio Cloudが狙う場合の差 |
| --- | --- | ---: | --- |
| [**Nasuni**](https://www.nasuni.com/press-release/nasuni-unveils-expanded-strategy-brand-and-platform-enhancements-for-file-data-activation-to-help-maximize-ai-investments-and-productivity/) | global namespace、permissions、versioning、cyber resilience。permission-aware MCPのAI Activateは2026年Q4 GA予定 | **最重要** | storage移行ではなく既存storage上へ重ね、回答を特定commitとclaim-level Evidenceへ固定する。予定機能を提供中と扱わない |
| [**Egnyte**](https://www.egnyte.com/products/ai-assistant) | secure content collaboration、権限準拠AI、versioning、[snapshot recovery](https://helpdesk.egnyte.com/hc/en-us/articles/4416718848397-Snapshot-Based-Ransomware-Recovery) | **最重要** | citationではなく、改名・移動・上書き後も解決するpointer、過去時点Q&A、固定knowledge releaseで差を作る |
| [**Box Hubs + Box AI**](https://blog.box.com/box-hubs-smarter-way-share-knowledge-across-your-organization) | curated scopeへのQ&A、引用、権限、source更新の反映。Box Shieldは[content recovery](https://support.box.com/hc/en-us/articles/37868517994899-Box-Shield-is-adding-advanced-ransomware-recovery-capabilities-Jan-2025)も提供 | 高 | 「常に最新」に対し、Pinned / Live / Releaseを明示し、共有時点と回答根拠を再現する |
| [**SharePoint Agents**](https://support.microsoft.com/en-us/sharepoint/copilot-in-sharepoint/get-started-with-agents-in-sharepoint) + [**OneDrive**](https://support.microsoft.com/en-US/onedrive/restore-your-onedrive) | site/library scope、質問者権限に応じた回答、共有、version/restore | 高 | Office UIや配布力と競わず、異種storage、content identity、長期historical evidenceへ集中する |
| [**Dropbox Dash**](https://dash.dropbox.com/) + [**Dropbox Rewind**](https://help.dropbox.com/delete-restore/rewind) | 複数SaaS横断検索、sourced answer、context-richなStacks、時点復元 | 高 | 横断検索と復元を、検索commit・claim evidence・生成profile・権限判断を含む検証可能なchainへ結ぶ |
| [**Glean**](https://www.glean.com/enterprise-search) / [**Notion Enterprise Search**](https://www.notion.com/help/enterprise-search) | 多数connectorを横断するcurrent-state search、権限準拠、出典付き回答 | 中〜高 | connector数で競わず、bounded project、raw履歴、`as-of` / `diff` query、external handoffを主戦場にする |
| [**Gemini Notebook**](https://support.google.com/gemininotebook/answer/16322204?hl=en) | source集合を共有し、受領者が原文を読み、AIへ質問する体験 | 中 | live更新か固定版かを明示し、raw digest、ACL、期限、answer receipt、時点差分を企業・技術用途向けに強化する |
| [**Rubrik**](https://www.rubrik.com/solutions/ransomware-recovery) / [**Cohesity**](https://www.cohesity.com/solutions/ransomware/) / [**Veeam**](https://www.veeam.com/solutions/data-security/ransomware-recovery.html) | immutable/isolated copy、異常検知、clean recovery、復旧運用 | 隣接基準 | 汎用backupとして競わない。Kioがsecurity claimを行う際の最低基準として比較する |

Nasuniは将来の構造が最も近く、EgnyteはAI・権限・共有・復旧を現在の製品体系で統合した強い比較対象である。この2社をCloud戦略の定期benchmarkとする。

## 4.4.3 競争判断 — 機能ではなく「履歴付き証拠workflow」に置く

次の各機能は必要になり得るが、単体ではmoatではない。

```text
- local / cloud RAG
- shared scopeへのAI Q&Aとcitation
- permission-aware retrieval
- version history / point-in-time restore
- MCP / Agent API
- content-addressed storage
- blockchain / external timestamp anchoring
```

将来Cloudの差別化仮説は、これらを次の一つのworkflowへ結ぶことである。

```text
Versioned source identity
  + explicit Pinned / Live / Release share
  + every answer bound to the searched commit
  + claim-level immutable Evidence Pointer
  + historical / as-of / diff query
  + permission-aware verification
  = reproducible knowledge handoff
```

このworkflowの高保証オプションとして、将来の **Externally Witnessed Snapshots** を検討する。CASはobject内容の不一致を検知できるが、運営者または攻撃者が保存領域全体を支配して別のsnapshot DAGへ作り替えた場合に、「以前の履歴が存在したこと」を外部へ単独では証明できない。その境界を補う採用候補は次の通りである。

```text
CAS + snapshot DAG
  -> signed snapshot/checkpoint envelope
  -> append-only Merkle transparency log
  -> independent witness + RFC 3161 timestamp
  -> optional aggregate public-chain anchor
  + separately governed WORM vault and restore drills
```

署名付きenvelope、transparency log、外部timestampは現行MVPおよび現行commit schemaの機能ではない。独自/private blockchainはdefault候補にせず、public chainを使う場合も第三者証人の一選択肢として、tenant、scope、user、path、query、回答本文、`raw_hash`、完全なEvidence Pointerを載せず、salt/nonceを含むdomain-separated leaf commitmentを集約したrootだけをanchorする。これは改竄の検知と存在時刻の外部検証を強めるが、改竄・削除・malwareを防止せず、原典やAI回答の正しさ、復旧可能性、RPO/RTOも保証しない。復旧用WORM copyとrestore drillは別のcontrolである。用語も「blockchain-powered」ではなく、検証要件を満たした場合に限り「cryptographically verifiable history」「externally witnessed snapshots」と表現する（[NIST Blockchain Technology Overview](https://www.nist.gov/publications/blockchain-technology-overview)、[RFC 9162](https://www.rfc-editor.org/rfc/rfc9162.html)、[RFC 3161](https://www.rfc-editor.org/info/rfc3161/)）。

最初のwedgeは、project終了時の**Pinned Snapshot Share**である。顧客・後任・共同研究者は固定されたknowledge releaseへAIで質問し、各claimの原文根拠を確認でき、作成者は後日「何を共有したか」を再現できる。Google Drive / OneDrive / NASを置き換えず、project単位で導入する。

Knowledge Continuityは同じCAS・commit・evidenceを利用できる第二の柱だが、現行のauto commitはindex/finalize成功時の履歴点であり、分離保管・常時保護・clean point判定・RPO/RTOを意味しない（[05-runtime.md §8.1](05-runtime.md)、[09-mvp-scope.md §2](09-mvp-scope.md)）。訴求は次のgateを満たしてから行う。

| claim | 使用条件 |
| --- | --- |
| automatic history | 対象となるindex/finalizeまたはschedulerのauto commitを実装・検証済み |
| point-in-time recovery | 対象scopeで復旧試験が成功し、保持範囲と制約を明示できる |
| cryptographically verifiable history | canonicalな署名対象、鍵の生成・rotation・失効、log inclusion/consistency proof、offline verificationを実装・相互運用試験済み |
| externally witnessed snapshots | 独立witnessまたはTSA receipt、split-view監視、公開commitmentとtenant snapshotの対応、privacy/purge手順を検証済み |
| cryptographically verifiable answer receipt | canonical receipt hashが回答本文・claim・Evidence Pointer・検索commit・policy/retrieval状態をbindし、issuer/serviceまたは委任signerの署名を外部検証可能。log inclusion/timestampは追加証跡であり署名の代替にしない |
| immutable/protected history | 通常のapp/admin credentialではretention中の上書き・削除ができない |
| ransomware-resilient | 別trust domainのvault、検知、containment、clean point選択、隔離復旧、定期演習が揃う |
| business continuity | RPO/RTO、runbook、依存serviceを含む復旧演習を提供する |
| blockchain-powered | **使用しない**。有効化した外部anchorの方式と保証範囲を具体的に説明する |
| ransomware-proof | **使用しない** |

将来Cloudの推奨category、wedge、優位性、非目標、claim gateの詳細は、非規範の戦略文書 [cloud-competitive-advantage.md](../strategy/cloud-competitive-advantage.md) を参照する。multi-device sync / cloud sharing自体は現行MVPの未承認roadmapである（[09-mvp-scope.md §2](09-mvp-scope.md)）。

## 4.4.4 Gemini Notebook / NotebookLMとの差別化 — citationとEvidence Pointerは別物

Gemini Notebookのcitationは「notebookへ登録されたsource集合の中で回答根拠へ戻る」体験であり、そのnotebookと共有linkのlifecycleに閉じる。Evidence Pointerは `commit / tree / raw_hash / chunk_hash / span` で根拠を固定するため、次の4点で体験が異なる: (1) **不変性** — 原本のrename・移動・削除・上書き後も、明示purgeまではpointerを解決できる。(2) **time-travel** — 過去の任意snapshot時点を検索・参照できる。(3) **ローカル原本回帰** — `kio open` でOS規定appの原本へ戻れる。(4) **任意folder横断** — upload用corpusを作らず、手元の全indexed scope（過去版・削除済みを含む）を対象にできる。

Gemini Notebookは「選んだsource集合を読み、質問する」共有体験の強い基準であり、Kioと併用可能である。KioがCloudへ進む場合も、単なる同等Q&Aではなく、共有mode、検索commit、claim-level Evidence、変更差分を明示できる場合にだけ差別化が成立する。

# 4.5 Perkeep 失敗分析 (Kio が学ぶべきこと)

Perkeep は思想的に Kio と最も近い (content-addressed、ローカル中心、所有権、永続保存)。にもかかわらず一般化していない。仮説:

```
- セットアップが技術者向け (server プロセス, blob 概念, importer)
- ユーザー体験が抽象的 (「保存できる」が日常的な体験差に変換されない)
- 既存ファイルシステムとの関係が曖昧 (どちらが正本か分かりにくい)
- 検索・閲覧・整理の即効性が弱い
- "Why now?" の訴求が時代と噛み合わなかった
```

Kio が同じ轍を踏まないための行動原則:

1. **最初の体験を即効的にする**: `kio init → kio index --approve → kio search "あの PDF" → kio open <pointer>` で価値が出る状態。「思想」を最初に売らない。
2. **ファイルシステムとの関係を明示**: 原本は元の場所にある。`.kio` は原本を置き換えない隠しアーカイブ層 (CAS コピー + metadata — 容量は原本相当 + 派生を見込む)。Perkeep のように「blob store を原本の置き場にする」ことはしない。
3. **content-addressed は手段、Evidence Pointer は目的**: ユーザーに blob/CAS を見せない。見せるのは「根拠が死なない」結果だけ。
4. **既存ワークフローに乗る**: Obsidian vault / Documents / Downloads を **置き換えず横断する** 外部アーカイブ層として始める (詳細 §8)。

# 4.6 重なる領域 = 相互運用 / 乗らない

| 領域 | 重なる相手 | Kio のスタンス |
| --- | --- | --- |
| 個人ノート vault | Obsidian | **置き換えない**。vault を含む親フォルダに `.kio`。vault 内検索は Smart Connections に任せ、Kio は vault + Documents + Downloads + コードを横断 |
| AI チャット UX | Khoj, AnythingLLM | **chat UI自体は競わない**。ローカル文書検索は第一代替として比較しつつ、Kioは履歴・EvidenceをCLIの構造化JSONで提供する |
| 大容量ファイル管理 | git-annex | **対象が違う**。git-annex は同期・バックアップ。Kio は知識検索と Evidence。両立可能 |
| 画面履歴 | Microsoft Recall, Rewind | **競合しない**。レイヤーが違う (画面 vs ファイル) |
| OS 統合 | Apple Intelligence, Windows Copilot | **競合しない**。OS ベンダーは横断アーカイブ層を提供しない |
| 文書アーカイブ | Zotero, Paperless-ngx | **置き換えない**。Zotero ライブラリ等を含む親フォルダに `.kio` を置き、専用アーカイブの外にあるファイルも含めて横断する |

# 5. MVP スコープ (絞り込み)

Kio は要素が多すぎるので、MVP では **一次・二次** を厳格に分ける。

### 5.1 MVP に含める (Phase 1〜3)

```text
content-addressed raw object 保存
Normalized Markdown (incremental Markdownize 含む)
chunk
Embedding
FTS (FTS5 外部 content + trigram tokenizer)
Hybrid search
Evidence Pointer
snapshot DAG (commit / tree)
restore
time-travel search (--at)
```

未実装機能の名称は [09-mvp-scope.md §2](09-mvp-scope.md) の非規範・非承認 roadmap
だけに置く。

---

# 6. Phase Plan (価値の柱の区分 — 実装順ではない。実装順の正本は [09-mvp-scope.md §3](09-mvp-scope.md) の Step 計画 / README §2)

```
Phase 1: Evidence 基盤
  - raw object 保存
  - normalized markdown 生成 (incremental 含む)
  - chunk 生成
  - Evidence Pointer

Phase 2: 検索
  - FTS5
  - sqlite-vec
  - hybrid search (paging / MMR)

Phase 3: 履歴
  - tree
  - commit / snapshot object
  - restore
  - --at / --all-history

```

---

# 7. 二層構造: truth と cache

データ・所有権・権限の正本は **各フォルダ直下の `.kio`** に閉じる。device-local な `scope_registry` と global aggregator は **検索キャッシュ** に過ぎない。device-global の例外は **cost-ledger.sqlite** (再構築不可の運用台帳 — cache ではない、[03-data-model.md §4.1](03-data-model.md))。

```
truth = folder-local .kio
  - raw object / normalized / chunks / commits / refs
  - purge の単位

cache = scope_registry
  - 検索の探索対象一覧 / stale 検出
cache = aggregator
  - 全 scope の chunk (live + 過去) を複製した device-level read replica
  - 横断検索の採点・候補選択、権限状態の横断投影
```

ルール:

- scope_registry / aggregator のみを更新して `.kio` の状態が変わる実装は禁止。
- scope_registry / aggregator 喪失は再構築可能 (各 `.kio` を rescan)。`.kio` 喪失は復旧不能。
- 検索結果メタには「正本の `.kio` パス」を必ず含める。
- **aggregator は安全性判定の最終権限を持たない** — 結果を返す scope は live `.kio` で再確認する
  ([05-runtime.md §1.8](05-runtime.md))。**権限の書き込みは常に `.kio` へ行う**。

---

# 8. 既存ワークフローとの関係

Kio は既存ツールを置き換えない。**横断する**外部アーカイブ層として始める。

| 既存ワークフロー | Kio の関係 |
| --- | --- |
| Obsidian vault | vault を含む親フォルダに `.kio`。vault 内検索は Smart Connections に任せ、Kio は vault + Documents + Downloads + コードを横断。 |
| Git リポジトリ | リポジトリ自体には `.kio` を置かない (Git に管理される)。`kio index` は VCS repo root 配下に既定で子 `.kio` を作らないため、**リポジトリ内のコードは既定では検索対象外** — コードも対象にするには `[scope] index_vcs_repos = true` の明示 opt-in ([03-data-model.md §3](03-data-model.md))。検索が横断するのは repo 外のファイルと他 scope である (横断検索は scope_registry 経由の全 scope 検索であり、親 `.kio` 自体は直下のみ管理 — 03 §3)。 |
| 既存ファイル整理 | Documents / Downloads など散らかった領域を整理せず、横断検索と Evidence で「整理しなくても見つかる」体験を提供。 |
| Khoj / AnythingLLM | ローカル文書検索では第一代替だが、chat UXは彼らに任せる。Kioの履歴・Evidenceを構造化APIから呼ぶ相互運用も狙う。 |

---

# 9. ポジショニングを揺さぶる発言禁止リスト

ドキュメント・README・ピッチでは、以下のフレーズを使わない。

```
✗ "Git for knowledge"
✗ "全部入りのナレッジ管理"
✗ "個人 AI アシスタント"
✗ "OS 級"
✗ "Knowledge Graph for personal data"
✗ "Notion / Obsidian キラー"
✗ "offline-first"           (誤解を招く。"local-first" を使う)
✗ "private AI" / "機密 AI"  (中心軸ではない)
✗ "データはあなたのマシンから出ない"  (デフォルト構成 (frontier AI) では偽。「保管と主権はローカル」と言い換える)
```

採用する語:

```
✓ Local-first knowledge archive, powered by frontier AI.   (core)
✓ データはローカル、計算は最強の AI を使う。                  (core 日)
✓ local-first                                                (データ主権の語として)
✓ Evidence-grounded local knowledge archive                  (二次表現)
✓ 原文根拠付きローカル知識アーカイブ                            (二次表現)
✓ time-travel knowledge navigation                           (履歴特性を語るとき)
✓ Evidence Pointer                                           (技術用語として)
```


---

# 10. このドキュメントの更新方針

ポジショニングは頻繁に変えない。MVP リリース → 一次ユーザー検証 → 拡張判断、の**節目でのみ更新**する。本書は「現時点の確定版」を保つ。揺らぎは git history で追える。
