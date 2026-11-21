# Kio Cloud: 競争優位性の仮説

> 基準日: 2026-08-23
>
> Status: **非規範・未承認の将来仮説**。本書は採用候補、検証仮説、exit gate を記す戦略資料であり、現行仕様を変更しない。
>
> 現行Kio機能ではない。Kio Cloud、外部共有、クラウド同期、SLA、ランサムウェア復旧を提供中であるとは解釈してはならない。

## 1. 結論

Kio Cloudを作るなら、既存storageを置き換える製品にはしない。

> **Kio Cloudは、既存storageの上に置く `Versioned Knowledge Layer` の採用候補である。**

長期の外部向け価値仮説は次の一文に置く。

> **Verifiable, recoverable knowledge for humans and AI.**

ただし、`recoverable`を外部表現として使用できるのは、§10の段階5 exit gateを通過した後だけである。段階1〜3では **Verifiable, versioned knowledge handoff for humans and AI.** を使用し、復旧能力を示唆しない。

最初に検証すべき中核は、次の三点を一つの体験として結ぶことにある。

1. `snapshot-bound sharing` — 共有対象をファイル置場ではなく、特定commitの知識状態へ固定する。
2. `claim-level / proof-carrying answer` — 回答全体でなく主張ごとに、raw source identity、Normalized Markdown上の検証可能なUTF-8 span、時点を添える。
3. `historical / as-of / diff query` — 「今の答え」だけでなく、任意時点の答えと時点間の差分を問い直せるようにする。

`Knowledge Continuity`（知識状態の復旧）は重要な第二段階である。最初の購買理由にはせず、共有に使う履歴基盤が後に復旧基盤となる、という順で検証する。

```text
existing storage
      │
      ▼
Versioned Knowledge Layer
  ├─ pinned snapshot share
  ├─ evidence-bound Q&A
  ├─ as-of / diff query
  └─ later: protected knowledge-state recovery
```

## 2. 何をmoatと呼ばないか

次の単体機能は、導入条件にはなっても競争優位（moat）にはならない。

| 単体機能 | moatにならない理由 | 必要なら置く位置 |
|---|---|---|
| AI Q&A | 共有コンテンツへの質問は既に一般化している | 回答の入口 |
| citations | 現行時点の資料リンクや引用は既存製品にもある | claim検証の素材 |
| permissions | 企業コンテンツ製品の前提能力である | access controlの下限 |
| version history | ファイル履歴・復元は広く提供される | 時点指定の基礎 |
| ransomware recovery | 専業製品は隔離・検知・演習まで扱う | 後続の信頼要件 |
| MCP | Agent接続は急速に一般化している | 読み取りAPIの配布経路 |

moat候補は、これらを同じcontent identityへ結び、共有時点・回答根拠・履歴照会・後日の検証を一貫させるワークフローである。

```text
"引用がある"                 → どの主張がどの原文に支えられるか
"版履歴がある"               → その版に固定して共有・照会・再現できるか
"AIが答える"                 → 回答時のcommitと根拠を後から検証できるか
"復元できる"                 → cleanな知識状態として安全に戻せるか
```

## 3. 現行Kioから得られる資産と境界

この仮説は、現行のローカル製品の正本をクラウド仕様へ読み替えるものではない。基盤資産と未解決の境界を区別する。

| 現行の根拠 | 活かせる資産 | クラウド化時に未承認のこと |
|---|---|---|
| [位置づけ](../01-positioning.md) | local-first、原文根拠付き探索、CLI MVPという焦点 | 企業向け基盤・クラウド共有は現行の対象外 |
| [データモデル](../03-data-model.md) | CAS、tree/commit、snapshot DAG、再生成可能な派生物 | tenant分離、鍵、remote object lifecycleは未規定 |
| [runtime](../05-runtime.md) | `--at`、全履歴、cursor時点固定、auto commit | 常時保護、SLO、隔離復旧、クラウド同期は未規定 |
| [adapter](../07-adapter-spec.md) | 明示opt-in、送信記録、frontier/local adapterの選択 | 共有受領者・組織AIの同意と送信境界は未規定 |
| [Evidence Pointer](../08-evidence-pointer-spec.md) | commit/raw/chunk/spanに結ぶ証跡 | share token、署名、失効、外部検証APIは未規定 |
| [MVP scope](../09-mvp-scope.md) | auto snapshot、履歴検索、purgeの段階導入 | cloud productのロードマップではない |
| [operations](../10-operations.md) | folder-local truth、registry/aggregatorはcache | multi-tenant運用、incident response、法的保持は未規定 |

特に、現行のtruth/cacheの分離を保持する必要がある。folder-local `.kio`、CAS、commitが正本であり、normalized view、index、scope registry、aggregatorは再構築可能なcacheとして扱われる。この考え方はクラウドでも有効だが、cloud copyを正本にするかは別途の製品決定である。

CASは保存されているobjectがhashと一致することを検証できる。一方で、store全体を支配する者が過去のobject・refs・DAGを削除し、別の自己整合的なDAGを再構築した場合、「以前に別の履歴が存在した」ことは内部hashだけでは外部へ証明できない。この限界を補う高保証の候補は、blockchainそのものではなく、外部から検証できる履歴である。

現行のauto commitはindex/finalize成功時に履歴点を作る設計である。これは「履歴付き」の出発点にはなるが、連続保護、攻撃検知、隔離、clean point判定、RPO/RTOの保証を意味しない。

また、現行のpurgeではEvidence Pointerとの整合性のためmetadata/tombstoneを扱う。クラウドでは法的削除、個人情報を含むpath、retention lock、legal hold、共有cache、回答ログとの緊張を明示的に解かなければならない。ここを「不変だから消せない」で済ませる案は採用候補にしない。Evidence PointerのspanはNormalized unit本文内のUTF-8 byte spanであり、raw byte spanではない。

## 4. 競争地図

以下は機能一覧ではなく、Kioがどの競争を避け、どこを比較対象に置くかの地図である。ベンダー資料の主張は独立検証済みの性能・安全性を意味しない。

| 層 | 代表競合 | 顧客が既に得ている価値 | Kioが正面競争を避ける対象 |
|---|---|---|---|
| Local | DocsAgent、Constella、Msty、Pieces | ローカル検索、RAG、MCP、個人/チームmemory | 「ローカル文書にAIで質問できる」だけの競争 |
| AI sharing/search | Box Hubs、SharePoint Agents、Dropbox Dash、Glean、Notion、Gemini Notebook | 権限連動検索、共有コンテンツへのQ&A、横断検索 | current-state shared-folder chatbot |
| content + recovery | Egnyte、Nasuni | コンテンツ協業、versioning、検知・復旧、統制 | content suite全体の置換 |
| cyber recovery基準 | Rubrik、Cohesity、Veeam | immutable vault、隔離、clean recovery、復旧運用 | バックアップ専業との即時同等性 |

**最重要ベンチマークはNasuniとEgnyte**である。両社は、ファイルデータ、権限、AI、復旧を同じ顧客予算に近付けている。Kioは「より良いストレージ」では勝負せず、既存ストレージを残したまま、版に固定された外部handoffとproofを提供できるかで比較されるべきである。

競争上の問いは次のように置く。

| 競合群 | 顧客の自然な比較 | Kioの勝ち筋仮説 | 反証となる事実 |
|---|---|---|---|
| Local tools | なぜKioなのか | 履歴と証跡が消えない探索 | 利用者がcurrent searchだけを求める |
| Box/Microsoft/Dropbox等 | なぜ既存共有で足りないのか | 受領者に「共有時点」を確実に渡し、回答を再現する | live共有と通常citationでhandoffが十分 |
| Egnyte/Nasuni | なぜ彼らを選ばないのか | 異種ストレージ横断で、軽量にversioned evidenceを重ねる | connector/ACL/復旧要件が同等に求められる |
| Rubrik/Cohesity/Veeam | 復旧を約束できるのか | 初期は約束しない。後段で知識状態の復旧を検証する | security buyerが初日から同等の保証を要求する |

## 5. 推奨wedge: Pinned Snapshot Share

最初のcomplete vertical slice候補は、**AI-native project handoffの `Pinned Snapshot Share`** である。

想定場面は、プロジェクト責任者が外部の顧客、監査人、後任、協力会社へ「この判断時点で渡すべき資料集合」を渡す時である。受領者はフォルダ構造を学習せず質問でき、回答の各主張を当該snapshotの原文まで追える。

```text
owner selects a commit
        │
        ▼
share = scope + pinned_commit + policy + expiry
        │
        ├─ recipient asks a question
        │       │
        │       ▼
        │  answer receipt = commit + claims + evidence pointers
        │
        └─ recipient verifies source identity + normalized span at that commit
```

このwedgeは、巨大なstorage migrationを要求しない。まず既存の資料をKioの版付き知識状態として取り込み、そのうち明示選択したcommitを読み取り専用で共有する。受領者のQ&Aが価値を作り、ownerにとっては「何を、いつ、どの根拠で渡したか」が残る。

### 5.1 競争優位のメカニズム

| 機構 | 顧客が感じる差 | 模倣への抵抗となる条件 |
|---|---|---|
| Pinned default | 「後で内容が変わった」共有事故を避ける | share semanticsがcommit中心で一貫する |
| claim-level proof | 回答を鵜呑みにせず、主張単位で検証できる | raw identity、normalized span、pointer、権限、表示UIが統合される |
| as-of / diff query | 当時の判断と後の変更を比較できる | 履歴indexとquery semanticsが実装される |
| evidence receipt | 回答を監査・handoff・再検討に使える | receiptの再解決と失効が可能である |
| storage overlay | 既存のDrive/Box/SharePointを即時置換しない | import/exportと導入摩擦の低さが保たれる |

差別化は「固定リンク」だけではない。固定した知識状態に対して、質問、主張、raw source identity、normalized view、時点、差分を同じモデルで扱えることが必要である。いずれかが欠ければ、単なる共有portalまたはRAG UIになる。

### 5.2 非目標

初期採用候補では、次を非目標として明示する。

- Google Drive、Dropbox、SharePoint、Boxのファイル編集・同期・office協業を置換しない。
- 汎用enterprise searchとしてconnector数を競わない。
- write-capable agent、共有先からの原本編集、自律実行を初期scopeに入れない。
- 「ランサムウェア対策済み」「immutable backup」と販売しない。
- citationがあることを回答の真実性保証として扱わない。
- cross-tenant dedupを初期最適化にしない。

## 6. ICPと購買仮説

最初のICP候補は、版・根拠・外部handoffが日常的に問題になる小規模な専門チームである。

| 優先度 | ICP候補 | 具体的な痛み | 初期の成功場面 |
|---:|---|---|---|
| 1 | 研究、開発、設計、コンサルのproject lead | 後任・顧客へ渡した時点と根拠を説明できない | milestone時の外部handoff |
| 2 | 規制・監査に近い専門サービス | 資料改訂後に当時の判断根拠を辿れない | review packageのQ&A |
| 3 | 複数organizationで作業するproject team | 相手ごとに資料探索・質問対応が繰り返される | 読み取り専用の受領者workspace |

初期に避けるべきICPは、全社検索の置換だけを求める巨大企業、即時のcyber recovery保証を求めるCISO主導案件、drive migrationが前提の案件である。いずれも必要な販売・統制・運用がwedgeを覆い隠す。

購買者は一人ではない。最初の価値訴求はproject/knowledge ownerに置き、security/ITは後にtrust reviewの当事者となる。二つの予算を一つの初期プランで同時に取りに行かない。

## 7. 採用候補のプロダクト原則

### 7.1 Share semantics

| mode候補 | 用途 | 初期の扱い |
|---|---|---|
| Pinned | 外部handoff、監査、承認、成果物引渡し | defaultとして検証 |
| Live | 継続中の内部協業 | Pinnedの価値が確認された後に検討 |
| Release | 明示承認済みの共有版 | policy/approvalモデル確立後に検討 |

外部共有には、少なくともscope、pinned commit、受領者権限、期限、失効、AI送信先、閲覧/質問ログの扱いを表せる必要がある。これは仕様の確定ではなく、shareを安全に語るための最小論点である。

### 7.2 Proof-carrying answer

回答は次の状態を区別する候補とする。

```text
supported     sourceに明示根拠がある
inferred      複数sourceからの推論である
conflicted    source間で矛盾がある
insufficient  根拠が足りない
unverified    pointer、permission、scan等を確認できない
```

`supported`でも、原典の事実が正しいことは保証しない。Kioが保証しようとするのは、回答がどのsnapshotのどの範囲に依拠したかを検証可能にすることである。

各claimに、少なくともsnapshot commit、Evidence Pointer、表示したspan、生成時刻、model/providerの識別、policy状態を結び付けられるかを検証する。Live shareでもanswer receiptは必ず具体的なcommitへbindする候補とする。

高保証trackを採用する場合、answer receiptには将来、answer digest、snapshot commit、permission/policy状態、retrieval manifest、Evidence Pointer、model/profileの各digestと、snapshot/checkpointのsignature、transparency logのinclusion/consistency proof、外部timestamp proofを含むproof bundleを追加する候補がある。ただしcheckpointのproofだけでは、回答本文、claim、pointer、model/policyとの結合は認証されない。これらを固定したと表現するには、canonical receipt bytesまたはそのhashへissuer/serviceまたは委任signerが署名し、そのreceiptとsnapshot/checkpointの結合までverifierが検証しなければならない。log inclusionやtimestampは追加のappend-only・時刻証跡であり、issuer認証の代替にしない。成立後も保証対象は入力・履歴・出典・回答本文の固定性であり、AIの真実性、原典の正しさ、snapshotのmalware-free性、復旧可能性ではない。

### 7.3 AccessとAI安全性

permission-aware accessは差別化でなく下限である。shareのACLを検索、retrieval、pointer resolve、source preview、answer receiptのすべてに一貫適用できなければ、外部共有は開始しない。

文書内のprompt injection、悪性リンク、権限境界を越えるretrieval、embedding/index漏えいは別途評価対象である。read-only Q&Aから始めるのは、価値を狭めるためでなく、書込みとagent実行が導入する取り消し不能な被害面を分離するためである。

## 8. Knowledge Continuityは第二段階

Knowledge Continuityの仮説は「ファイルを戻す」より広い。cleanなcommit、原本、検索可能な派生物、共有可能な証跡を含む知識状態を、検証済みの経路で戻せることを指す。

しかし、snapshot DAGやCASがあるだけでは成立しない。同一credentialや同一管理領域で消せる履歴は、攻撃から分離されていない。cyber recoveryを訴求する前に、少なくとも以下をexit gateとする。

| claim候補 | 必要なexit gate | gate不通過時の表現 |
|---|---|---|
| 履歴から復元できる | 任意commitのobject整合性検証と復元テスト | 「履歴閲覧」 |
| 破壊的変更から戻れる | retention、権限分離、復元runbook、定期演習 | 「復元設計を検討中」 |
| ransomware recovery | 隔離trust domain、不変保管、異常/clean point判定、incident演習 | 主張しない |
| business continuity | RPO/RTO、support、責任分界、監査可能な実績 | 主張しない |

将来的なprotected vaultは、production tenant/projectと別trust domainに置く候補とする。WORM/retention lockを採用する場合も、法的削除、retention上限、lockの誤設定、緊急時の権限分離までを含めて評価する。

復旧候補へmalware scan等のsecurity attestationを付ける場合は、対象commit、scanner/profile、定義version、結果、実行時刻、signerを署名対象へbindする候補とする。attestationは「その時点の構成でその結果が記録された」ことを示すだけで、scanの検出精度やsnapshotの安全性を暗号学的に保証しない。

WORMや外部anchorはランサムウェアを阻止せず、clean point、scan efficacy、RPO/RTOも保証しない。このためKnowledge Continuityのclaim gateをこの高保証trackで代替しない。

## 9. 高保証track: Externally Witnessed Cryptographic History

これは現行機能ではなく、顧客opt-inの未承認候補である。既存の価値ロードマップを実装Phaseへ置換せず、次のassurance progressionとして並走評価する。目的は「blockchain搭載」ではなく、Kio運営者を含む単一store支配者が過去を密かに差し替えにくい、verifiable historyとanswer workflowである。

| 順序 | trust stack候補 | 検証できること / 境界 |
|---:|---|---|
| 1 | CAS | object整合性。過去全削除＋自己整合的DAG再構築は外部証明できない。 |
| 2 | signed snapshot/checkpoint envelope | 作成主体と固定されたsnapshot/checkpoint。現行commit schemaの変更を意味しない。 |
| 3 | Merkle transparency log | inclusion proofとconsistency proofによるappend-only性の検証候補。 |
| 4 | independent monitor/witness | checkpointを独立に観測・共有し、split-viewを検知する候補。 |
| 5 | RFC 3161をdefault、public-chain/OpenTimestamps aggregate-root anchorはopt-in | checkpointが時刻以前に存在したことの外部証明候補。独自/private blockchainを既定で作らない。 |
| 6 | 別trust domainのWORM vault + restore drill | object保持・復元経路を評価する候補。復旧保証ではない。 |

署名層の採用には、canonical envelope、signer identity、鍵の生成・rotation・失効、鍵侵害時の扱い、offline verificationが必要である。log層にはinclusion/consistency proofと独立checkpoint観測が必要であり、本書では具体的な署名payload、CLI、anchor cadenceを確定しない。

公開anchorには、salt/nonceを含むdomain-separated leaf commitmentを集約したrootのみを候補とする。tenant、scope、user、path、query、answer、個別raw hash、Evidence Pointer全文を載せない。pseudonymisationは匿名化ではなく、外部anchorと削除要件の意味論・法務評価が通るまで有効化しない。現行purgeのtombstone/not_found等の意味論を上書きせず、「過去にcommitmentが存在した」こととcontentを削除する要件の関係をexit gateにする。

このtrackは既存の段階2のclaim receiptを強化し得るが、段階4のseparated protection prototypeや段階5のcontinuity販売判定を飛ばさない。外部証人は履歴の証明を補強し、clean recoveryの運用保証を置き換えない。

## 10. 段階的ロードマップとclaim gate

ロードマップは実装順序の決定ではなく、価値の検証順序である。各段階はexit gateを満たすまで次の市場claimへ進まない。

| 段階 | 仮説 | 最小検証物 | exit gate | 許容する外部表現 |
|---:|---|---|---|---|
| 0 | ローカル資産がhandoffに使える | historical search、pointer、commit表示 | 現行機能の整合性とユーザー調査 | local archive |
| 1 | Pinned shareに独自価値がある | 読み取り専用share + expiration | 受領者が資料探索なしで回答/根拠に到達 | design partner preview |
| 2 | proofがcitationより強い | claim receipt + verify UI | claim単位で再現・権限確認・失効を通過 | verifiable handoff |
| 3 | 時間軸が継続利用を生む | as-of / diff query | change reviewで通常共有より短時間/低誤解 | versioned knowledge layer |
| 4 | shared stateが復旧に拡張できる | separated protection prototype | clean restore演習と責任分界を検証 | limited continuity preview |
| 5 | continuityを販売できる | 運用、SLO、incident runbook | 実運用の復旧証跡・外部レビュー | recoverable knowledge |

段階1〜3の間、Kioはstorage replacementでもbackup製品でもない。段階4以降も、Rubrik/Cohesity/Veeam級の保証を比較可能に示せない限り、ransomwareのmarketing claimは出さない。

## 11. 失敗条件と反証

この戦略は魅力的に見えるが、次のいずれかが観測されたら縮小・方向転換を検討する。

| 反証/失敗条件 | 何を意味するか | 推奨対応 |
|---|---|---|
| 受領者がAI Q&Aを使わずPDFをdownloadする | share UXが既存手段を超えていない | ICP/手渡すunitを再調査 |
| PinnedよりLiveを一貫して求める | 時点固定がworkflowに適合していない | Liveを先にしない。判断文脈を分解 |
| citationsだけで十分と評価される | claim-level proofの価値が見えていない | verification UIとuse caseを再設計 |
| ownerがcommit選択を理解できない | snapshot modelが操作負荷になっている | release/presetの表現を試す |
| 導入にstorage migrationや全社IAM統合が必須 | overlayの利点が消えている | scopeをproject単位へ戻す |
| security reviewで隔離・削除設計が通らない | continuityの信頼境界が未成熟 | recovery claimを延期 |
| 外部証人のprivacy/purge評価が通らない | anchorが削除要件または顧客信頼と両立しない | high-assurance trackを有効化しない |
| monitor/witnessがsplit-viewまたは検証失敗を検知する | 履歴保証の前提が崩れた | 当該保証claimを停止し、調査・再発行する |
| Nasuni/Egnyte等が同等のsnapshot proofを提供する | 比較優位が縮小した | adapter/format/recipient workflowへ再集中 |

特に「Evidence Pointerがあるから正しい」という誤解は失敗条件である。証跡は検証の入口であり、原典の品質、鮮度、悪意、矛盾を消去しない。

## 12. Success metrics

metricsは利用量だけでなく、共有した知識が検証・再利用・復旧に値したかを測る。

| 層 | 指標候補 | 成功の読み方 |
|---|---|---|
| activation | 作成したPinned shareのうち、期限内に受領者が質問した比率 | shareが単なるdownload linkでない |
| answer quality | answer receiptを開いた比率、claimごとのevidence到達率 | 根拠UIが使われる |
| handoff efficiency | 初回質問までの時間、ownerへの追加質問数、handoff完了までの往復数 | 資料探索・説明の負担が減る |
| time-aware value | as-of/diff queryの利用率、変更判断での再利用率 | versioningが実際の意思決定を支える |
| trust | 権限逸脱ゼロ、失効後アクセスゼロ、検証失敗率 | 外部共有の最低品質を保つ |
| high assurance (opt-in) | 署名・inclusion・consistency・timestamp proofの検証成功率、monitor/witnessのcoverage、anchor遅延 | 外部証人付き履歴が運用可能かを測る |
| cost | share当たりstorage/AI/support費、再index費 | overlayが経済的に持続する |
| continuity (後段) | restore演習成功率、clean point確認率、実測RPO/RTO | sales claimを支える証跡になる |

評価は、同じprojectでの通常共有（共有フォルダ、PDF package、既存portal）との比較を含める。絶対利用数だけで「proofが価値を作った」と結論付けない。

## 13. 四半期更新ルール

本書は非規範資料として、少なくとも四半期ごとに更新候補をレビューする。

1. NasuniとEgnyteを最優先に、AI、MCP、権限、共有、復旧の公開機能を再確認する。
2. Box、Microsoft、Dropbox、Glean、Notion、Gemini Notebookのshared Q&Aとcitation体験を比較する。
3. DocsAgent、Constella、Msty、Piecesのlocal/MCP能力を比較し、local moatの陳腐化を確認する。
4. Rubrik、Cohesity、Veeam、CISA/NISTの復旧基準を見直し、Kioの表現が過大になっていないかを確認する。
5. design partnerの成功指標と反証を更新し、未通過gateのclaimを追加しない。
6. 仕様変更が必要になった場合は、本書を直接規範化せず、identity/access、share/evidence、retention/purge、continuity/recoveryを別の承認済み仕様へ分ける。
7. high-assurance trackについて、署名鍵・monitor/witnessの独立性、RFC 3161/anchorの検証率、privacy/purgeの法務評価、vault restore drillを確認し、未通過の保証を表現しない。

更新時には、基準日、変更した競合の公開情報、未検証のベンダー主張、Kioの実測値と仮説を分離して記録する。

## 14. 意思決定要約

| 判断 | 推奨 | 理由 |
|---|---|---|
| category | Versioned Knowledge Layer | storage replacement競争を避ける |
| first wedge | Pinned Snapshot Share | AI-native handoffを一つの完結体験として検証できる |
| share default | Pinned | 再現性、誤共有防止、証跡に合う |
| answer primitive | claim-level proof-carrying answer | citation付きQ&Aから差を作る |
| temporal capability | historical / as-of / diff | Kioの履歴資産を顧客価値へ変える |
| continuity | 第二段階 | security市場の信用・運用要件を先送りせず検証する |
| high-assurance history | customer opt-inのassurance progression | blockchainではなく外部証人付きの検証可能な履歴・回答workflowを強化する |
| benchmark | Nasuni / Egnyte | AI・共有・復旧の統合競合として最も重要 |

この仮説が成立する条件は、受領者が「最新資料を検索する」だけではなく、「その時点に何が根拠だったか」を短時間に検証したいことである。その需要を確認できなければ、Kio Cloudは作らず、現行のlocal knowledge archiveを強化する方が合理的である。

## 15. 公式一次ソース

以下は本書の競合・基準情報を確認するための公式一次ソースである。各社の機能、性能、ロードマップ、安全性に関する主張はベンダー自身の表明であり、独立検証ではない。

1. [DocsAgent repository](https://github.com/docsagent/docsagent)
2. [Constella Desktop repository](https://github.com/Constella-OS/constella-desktop)
3. [Msty Knowledge Stack Basics](https://docs.msty.app/features/knowledge-stack/basics)
4. [Pieces Enterprise AI Memory](https://pieces.app/enterprise)
5. [Box Hubs FAQ](https://support.box.com/hc/en-us/articles/40054879427731-Box-Hubs-Frequently-Asked-Questions)
6. [Microsoft: SharePoint Agents](https://support.microsoft.com/en-us/sharepoint/copilot-in-sharepoint/get-started-with-agents-in-sharepoint)
7. [Dropbox Dash](https://dash.dropbox.com/)
8. [Glean Enterprise Search](https://www.glean.com/enterprise-search)
9. [Notion Enterprise Search](https://www.notion.com/help/enterprise-search)
10. [Google Gemini Notebook sharing](https://support.google.com/gemininotebook/answer/16322204?hl=en)
11. [Egnyte AI Assistant](https://www.egnyte.com/products/ai-assistant)
12. [Egnyte snapshot-based ransomware recovery](https://helpdesk.egnyte.com/hc/en-us/articles/4416718848397-Snapshot-Based-Ransomware-Recovery)
13. [Nasuni platform](https://www.nasuni.com/)
14. [Nasuni AI Activate announcement](https://www.nasuni.com/press-release/nasuni-unveils-expanded-strategy-brand-and-platform-enhancements-for-file-data-activation-to-help-maximize-ai-investments-and-productivity/)
15. [Nasuni ransomware protection](https://www.nasuni.com/product/ransomware-protection/)
16. [Rubrik ransomware recovery](https://www.rubrik.com/solutions/ransomware-recovery)
17. [Cohesity ransomware recovery](https://www.cohesity.com/solutions/ransomware/)
18. [Veeam ransomware recovery](https://www.veeam.com/solutions/data-security/ransomware-recovery.html)
19. [CISA StopRansomware Guide](https://www.cisa.gov/stopransomware/ransomware-guide)
20. [NIST SP 1800-11: Recovering from Ransomware](https://www.nccoe.nist.gov/publication/1800-11/VolB/)
21. [AWS S3 Object Lock](https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-lock.html)
22. [OWASP LLM01: Prompt Injection](https://genai.owasp.org/llmrisk2023-24/llm01-24-prompt-injection/)
23. [OWASP LLM Verification Standard](https://owasp.org/www-project-llm-verification-standard/LLMSVS-v2.0-en.html)
24. [NISTIR 8202: Blockchain Technology Overview](https://www.nist.gov/publications/blockchain-technology-overview)
25. [RFC 9162: Certificate Transparency Version 2.0](https://www.rfc-editor.org/rfc/rfc9162.html)
26. [Sigstore Rekor CLI](https://docs.sigstore.dev/logging/cli/)
27. [RFC 3161: Internet X.509 Public Key Infrastructure Time-Stamp Protocol](https://www.rfc-editor.org/info/rfc3161/)
28. [OpenTimestamps](https://opentimestamps.org/)
29. [Google Cloud Storage: Bucket Lock](https://docs.cloud.google.com/storage/docs/using-bucket-lock)
30. [ICO: Pseudonymisation](https://ico.org.uk/for-organisations/uk-gdpr-guidance-and-resources/data-sharing/anonymisation/pseudonymisation/)
