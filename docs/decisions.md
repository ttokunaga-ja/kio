# 指示と決定の記録

利用者の指示と決定は、この repo ではこのファイルだけで管理する。

- 決まったことは、その場でここに書く。会話記録を根拠にしない。
- 1 件ごとに、日付、内容、理由、関係する資料を書く。番号は `D-<日付>-<連番>` とし、日付順に末尾へ足す。
- 製品の仕様そのものは `docs/01`〜`docs/13` が正本で、役割・モデル・停止の基準は `AGENTS.md` が正本である。ここには「何を決めたか」と理由だけを書き、仕様の本文を写さない。
- 後の決定で変わったときは、古い記録を消さず、その記録に「D-… で変更」と書き足す。

## 2026-11-12 より前の決定（旧資料から移したもの）

2026-11-13 に `tasks/` の旧資料を削除したとき、今も有効な決定をここへ移した。出典の commit は、その決定が記録された commit である。旧資料の本文は Git の履歴（`56a6831` 以前の `tasks/`）で読める。

### D-20260724-01 有料の処理は Batch を既定にする

- **内容**：OCR と embedding の有料処理は Batch を既定とし、realtime は `--realtime` を明示したときだけ使う。realtime を既定にはしない。
- **理由**：費用を抑えるため。
- **関係する資料**：`docs/04-pipeline.md`、`docs/06-cli-spec.md`。`tasks/step4b-backlog.md (git 56a6831)` に 2026-07-24・25 の裁定として記録されていた（承認の原記録は未確認）。

### D-20260907-01 v1・v2・v3 の境界

- **内容**：v1 は CLI の検索・保存・承認・自動管理・復旧を完成させる。GUI は v2、cloud・共同編集・利用者やグループごとの権限は v3 とする。
- **理由**：製品の世代の境界を固定するため。
- **関係する資料**：`docs/11-product-requirements.md`。commit `f198c26`。

### D-20260907-02 personaCorpus による評価は v1 の完了を妨げない

- **内容**：personaCorpus を使った品質・性能の評価は、v1 の完了条件にしない。機能・security・復旧の受入は維持する。
- **理由**：評価用の corpus の完成待ちと、製品の完成を切り離すため。
- **関係する資料**：`docs/09-mvp-scope.md`、`docs/11-product-requirements.md`。

### D-20260907-03 変更の検出は OS のイベントと共通の突合せで行う

- **内容**：変更の検出は OS のイベントと共通の reconciliation を基本とし、起動・再接続・overflow・定期走査で取りこぼしから回復する。
- **理由**：ディレクトリのサイズや更新時刻、イベントだけでは、漏れがないことを保証できないため。
- **関係する資料**：`docs/12-change-detection.md`。

### D-20260907-04 過去の状態の復元は、現在の HEAD の新しい子として記録する

- **内容**：復元は履歴を巻き戻さず、現在の HEAD の子として記録する。分岐を製品の要件にしない。
- **理由**：その後の履歴と現在の承認を保つため。
- **関係する資料**：`docs/13-linear-history.md`。

### D-20261003-01 v1 の後の改善の順番

- **内容**：v1 の受入の後、版ごとの状態の表示、既存の tag による引用の保持、復元の操作の表示の順で改善する。これらを v1 の完了の待ち条件にしない。保存の基盤を jj に置き換えること、分岐・rebase、内部ライブラリへの依存は、この工程に含めない。
- **理由**：v1 を閉じつつ、保存・検索・復元の状態を分かりやすくするため。現在の CAS・Evidence Pointer・線形の履歴を保つため。
- **関係する資料**：`tasks/post-v1/knowledge-ux.md`、`docs/11-product-requirements.md`。

### D-20261003-02 正式な受入の接続経路

- **内容**：Tailscale の接続ルールの縮小、CI 用の OIDC、main に限定したローカル受入の Environment を承認した。2026-10-04 に、Windows・WSL の専用 account、SSH の経路、dispatcher を計画の対象に加えた。
- **理由**：既存の接続を保ちながら、CI からの経路を限定するため。
- **関係する資料**：`tasks/v1-acceptance.md`、`scripts/v1-local-gpu/README.md`。外部の設定の現在の状態は、正式な受入の前に確かめる。

### D-20261003-03 有料 provider の受入の予算

- **内容**：正式な受入で使う有料 provider の費用は、provider ごとに累計 10 米ドルまでとする。失敗と結果不明の実行も数える。配分は 1 つの OS につき 0.10 米ドル、provider ごとに 3 つの OS で 0.30 米ドル。
- **理由**：受入の費用に上限を置くため。
- **関係する資料**：`tasks/acceptance/provider-budget.md`（承認済みと記載。承認の原記録と日付は未確認で、ここの日付は受入の計画を作った日である）。
- **確認**：2026-11-13、利用者がこの内容で承認済みであることを確認した。

### D-20261006-01 構造の是正と仕様書の整理を v1 に含める

- **内容**：OS 固有の層の分離、`kio-app` の分解と型付き API、巨大ファイルの分割、規模の計測を v1 に含める。仕様書から実装の内部の手順・重複・日付付きの経緯を除き、横断的な実行時契約を対応する仕様書へ統合する。
- **理由**：GUI も同じ Rust の engine を使える構造にするため。契約の所在と読み始める場所をはっきりさせるため。
- **関係する資料**：`docs/09-mvp-scope.md`、`docs/README.md`。構造の是正は実装済み。仕様書の整理は `tasks/v1-plan.md` の F2。

### D-20261006-02 受入の証拠を再利用できる条件

- **内容**：受入の証拠は、証拠の commit が現在の commit の祖先で、その後の差分が許可された path だけのときに再利用できる。binary の digest の一致では判定しない。
- **理由**：binary には commit と tree が埋め込まれ、文書だけの変更でも digest が変わるため。
- **関係する資料**：`crates/kio-eval/src/acceptance_reuse.rs`。

### D-20261018-01 共通コードの行数の上限

- **内容**：共通コードの上限は、2026-10-10 の決定で 100,000 行から 106,000 行に一度引き上げた（`284c836`）。embedding の作業（EG2 まで）が終わった時点の実測から、もう一度だけ引き上げる。
- **実行結果**：2026-11-20、EG2 完了時の実測 139,211 行をもとに、上限を 140,000 行に引き上げた。
- **理由**：型付きの response や明示的な context を、行数を減らすために崩さないため。EG2 は別の機能の追加で、既存の上限を超えるため。
- **関係する資料**：`docs/09-mvp-scope.md`、`crates/kio-ci/src/code_limits.rs`。`tasks/v1-plan.md` の F1。

### D-20261019-01 既定のローカル embedding を EG2 にする

- **内容**：既定のローカル embedding を EG2 とする。runtime は配布物に同梱し、モデルだけを明示的な取得か import で入れる。常駐の daemon は持たず、コマンドの終了時に worker とメモリを解放する。同じ semantic profile なら、OS の違う producer が作った vector を混ぜて検索できるようにする。Linux・CPU を認定する関門は設けず、品質の差が基準に届かなければ判断を戻す。
- **理由**：外部の server・Python・証明書の準備を、一般の利用者に求めないため。
- **関係する資料**：`docs/07-adapter-spec.md`、`docs/10-operations.md`、`docs/11-product-requirements.md`。設計の経緯は repo の外の `~/archive/mac-20261112/home/Documents/Codex/2026-10-18/kio-embed-spec/runs/A1/decisions-final.md`。

### D-20261019-02 ローカルの接続窓は残して汎用にする

- **内容**：ローカルの endpoint への接続は削除せず、OpenAI 互換の input 形式と messages 形式を選べる汎用の窓にする。
- **理由**：既にあるローカルの server を、共通の pipeline から使えるようにするため。
- **関係する資料**：`docs/07-adapter-spec.md`。

### D-20261022-01 制約の緩和（W1）

- **内容**：Kio が自分に課していた制約 100 件を調べ、62 件を緩和・削除すると決めた。優先度の高いものから段階的に進め、判断が要る 7 件は推奨案で進める。主な内容は次のとおり。保存の保護は Kio が作った物だけに限り、利用者のフォルダや Kio が作っていない祖先の権限は審査も修復もしない。コマンドは実際に使う設定・資格情報・CA・台帳だけを検査し、無料のローカル処理は課金の台帳なしで動く。loopback では HTTP を許し、未認証であることを表示する。固定の上限は、警告・逐次処理・資源の段階に置き換える。
- **理由**：無関係な依存の不備や、既定の権限の違いで、自分の保存領域や検索まで使えなくなる問題をなくすため。破損・誤った引用・無断の送信・二重の課金・実行コードの乗っ取りへの保護は変えない。
- **関係する資料**：commit `4233941`、`6656312`（仕様書の変更）。一覧は `tasks/post-v1/constraint-backlog.tsv`。v1 に入れる範囲は D-20261113-09。

### D-20261031-01 中断後の一時領域を識別する方式

- **内容**：中断した書き込みの一時領域を、物理的な ID と 256 bit の乱数の組で識別する。乱数は、Unix では拡張属性、Windows では EA に書く。対応していない場合に、別の方法へ切り替えて続けることはしない。
- **理由**：物理的な ID だけでは、再利用されたときに別の実体を取り違える恐れがあるため。
- **関係する資料**：`1b46cbb5:tasks/v1-plan.md` §3.2 の R1、`~/archive/mac-20261112/home/Documents/Codex/2026-10-22/kio-relax/runs/W1/report-r33-progress-ja.md`。利用者が「推奨の方式で再開」と指示した記録は Codex の会話記録にあり、承認の文面そのものは確かめていない。main には実装されていない。
- **確認**：2026-11-13、利用者がこの方式（対応していないファイルシステムでは、別の方法へ切り替えずに理由を示して失敗させる）で R1 を実装することを確認した。

## 2026-11-12

### D-20261112-01 作業場所と、Claude・Codex の役割

- **内容**：kio の作業は個人デスクトップの WSL で行う。方針・計画・仕様・規則の見直しは Claude Fable 5.2、開発・監査・検証の統括は Claude Opus 5.5、実装と定型の検証は Codex が受け持つ。kio には常駐の監視を置かない。
- **理由**：Mac から移行したため。役割ごとにモデルを分け、Claude の利用枠を判断に使うため。
- **関係する資料**：`AGENTS.md`「エージェント運用」（役割・モデル・停止の基準の正本）。

## 2026-11-13

### D-20261113-01 文書単位の検索結果は v1 に入れない

- **内容**：検索結果を 1 文書 1 行に畳む変更（文書名の時点規則、`changed_at` の追加、`snippet` の廃止を含む）は v1 後に回す。v1 の検索結果は、別名を展開した後の chunk 行のままとする。
- **理由**：2026-08-11 に方針を決めたが実装されておらず、仕様書の中で現行の契約と食い違っていた。v1 を閉じることを優先する。
- **関係する資料**：`docs/05-runtime.md` §1.7・§1.7.1、`tasks/post-v1/search-result-presentation.md`。

### D-20261113-02 v1 の完了条件を二段に分ける

- **内容**：「実装完了」は、WSL でのローカル検証と、通常の CI で 3 つの OS が成功することで判定する。51 件の正式な受入は、候補を固定した最後に一度だけ回す。51 件そのものは減らさない。
- **理由**：正式な受入は外部の資源（有料 provider、GPU の経路、3 つの OS）を使い、commit が変わるたびに証跡を取り直す作りになっている。実装の途中でこれを完了の判定に使うと、作業が閉じない。
- **関係する資料**：`tasks/v1-plan.md` §2、`.github/workflows/ci.yml`、`.github/workflows/v1-acceptance*.yml`。

### D-20261113-03 Phase 4 の手動受入契約（RC.1 用）は履歴扱い

- **内容**：`1b46cbb5:tasks/phase4-manual-acceptance-contract.md` の手順は v1 の完了条件にせず、今後は実行しない。
- **理由**：`v0.1.0-rc.1` に固定した手順である。M8 を必ず `blocked` と記録する規定があり、全段階の合格を求める最終条件と両立しない。
- **関係する資料**：`1b46cbb5:tasks/phase4-manual-acceptance-contract.md`、`1b46cbb5:scripts/phase4-replay-m1-m8.sh`、`1b46cbb5:scripts/phase4-bind-rc-release.sh`。

### D-20261113-04 ローカルにしかなかった branch 2 本を削除する

- **内容**：`codex/kio-rc-post-release-hardening`（`1ac8e3d`）と `codex/kio-storage-policy-20261005`（`f5e56b0`）を削除した。あわせて、main にマージ済みの `win/own-objects-only` と、その worktree `../kio-r2` を片付けた。
- **理由**：前者は 8 月の RC 監査メモだけ、後者は Mac を前提にした容量の規則を足す未マージの変更で、どちらも現行の計画に使わない。Mac の `.git` 全体は `~/archive/mac-20261112` に退避してある。
- **関係する資料**：merge commit `22a0c73`。

### D-20261113-05 未追跡の引き継ぎメモを処分する

- **内容**：`tasks/post-migration-fixes-brief.md` を削除した。p05・p07 の引き継ぎ 3 件は personaCorpus の記録なので、`~/archive/mac-20261112/home/dev/github.com/ttokunaga-ja/kio/tasks/codex-session-briefs/` へ移した。
- **理由**：どれも 8 月時点の作業メモで、kio の現行の計画に使わない。

### D-20261113-06 指示と決定はこのファイルだけで管理する

- **内容**：利用者の指示と決定は `docs/decisions.md` に集める。`tasks/` などに散らばった過去の決定のうち今も有効なものをここへ移し、古い資料は履歴扱いにするか削除して、計画の入口を 1 つにする。
- **理由**：決定が約 180 の資料と会話記録に散らばり、現状が資料から読めなくなっていた。

### D-20261113-07 Codex の作業場所は repo の中に限る

- **内容**：Codex の作業、出力、一時ファイルは repo の中の決まった場所か `$TMPDIR` に限る。`~/Documents`、ホーム直下、`~/dev/github.com/ttokunaga-ja` 直下には作らない。Codex への指示書には出力先を必ず書く。
- **理由**：Mac では repo の外に 130GB 以上の作業フォルダと結果が溜まり、どれが必要かを後から判断できなくなった。
- **関係する資料**：`AGENTS.md`「エージェント運用」。

### D-20261113-08 repo の外にあるものを整理する

- **内容**：`~/dev/github.com/ttokunaga-ja` 直下と `~/archive/mac-20261112` を整理する。再生成できるものと、repo や GitHub に同じものがあるものは削除する。会話記録は、決定の経緯と遅れの原因だけを取り出して、生の記録は削除する方向とする。実験の結果と証跡は、今の計画と判断の根拠に使うものだけを残す。再生成できるものとキャッシュ以外は、削除の前に一覧を利用者に示す。
- **理由**：移行で退避したものが大きく、要るものと要らないものが混ざっている。

### D-20261113-09 制約の緩和の残りは、優先度の高い 12 件だけを v1 に入れる

- **内容**：W1 の残り 53 件のうち、優先度が高い 12 件（ST05、ST04、ST06、AD-14、AD-16、AD-29、AD-30、APP-26、APP-11、CLI-01、AD-17、PROC-01）を v1 に入れる。中・低の 41 件は v1 の後に回す。仕様書は戻さない。
- **理由**：高の 12 件は、仕様書が 2026-10-22 に緩和後の内容へ書き換わっており、実装と食い違っている。中・低まで入れると v1 が閉じない。
- **関係する資料**：`1b46cbb5:tasks/v1-plan.md` §3.2、`tasks/post-v1/constraint-backlog.tsv`（「今回の整理」の列が「残件候補」の 53 行のうち、優先度が「高」の 12 行が v1、「中」の 23 行と「低」の 18 行が v1 後）。

### D-20261113-10 v1 の範囲を固定する

- **内容**：v1 の範囲は当時の計画（`1b46cbb5:tasks/v1-plan.md` §3）で固定する。v1 に何かを足すのは、利用者が決めたときだけとする。それ以外の追加・改善・見つかった課題は、すべて「v1 後」に回して `tasks/v1-plan.md` §6 に書く。現行の仕様に対する不具合の修正は、範囲の追加ではない。
- **理由**：遅れの一番の原因は、受入が閉じる前に範囲が広がったことだった（2026-10-06、10-18〜19、10-22 の 3 回）。
- **関係する資料**：`tasks/v1-plan.md`。

### D-20261113-11 repo の外の削除と、残すもの

- **内容**：D-20261113-08 の方針に沿って、archive とホームの不要なものを削除した。Codex の復旧バックアップ（19GB）は、personaCorpus の中央調整の会話が混ざっている可能性があるので削除せず、personaCorpus 側の判断に回す。personaCorpus に関わるものは kio のセッションでは手を付けず、`~/dev/github.com/ttokunaga-ja/personaCorpus-handoff-from-kio.md` に書いて残す。評価用の corpus（`kio-dogfood` の `corpus-v1` と最新の fixture、`kio-baseline-corpus` 2 個）は `~/archive/mac-20261112/home/` に原本として残し、評価で使うときは repo の `scratch/` に写して使う。v1 のテストと受入は、これらを使わない。
- **理由**：評価用の corpus は大きく（約 2GB）、公開の repo に入れるものではない。v1 の完了条件でもない。
- **関係する資料**：削除したものと残したものの一覧は `~/archive/mac-20261112/slimming-20261113.md`。

### D-20261113-12 `tasks/` の旧資料を削除する

- **内容**：`tasks/` の旧資料 109 ファイルを削除し、計画の入口を `tasks/v1-plan.md` の 1 本にした。残したのは、計画、制約の一覧、任意の高コスト検証の手順、v1 後の設計 2 本、正式な受入の経路の資料、履歴扱いの Phase 4 契約、テストが節番号で引用している試験仕様 6 本（`1b46cbb5` の `tasks/` に保存）、スクリプトとテストの説明が参照する `artifacts/` の一部、repo の `persona-corpus/` が参照する `persona-skill-corpus/` である。削除した資料を理由の出典として引用しているコメントは、`tasks/<名前>.md (git 56a6831)` と書き換え、その commit で読めるようにした。
- **理由**：D-20261113-06。試験仕様は約 190 か所のテストとコードから引用されており、削除すると引用先がなくなる。`persona-corpus/` と `persona-skill-corpus/` は personaCorpus の見直しで扱う。（D-20261115-02 で変更）
- **関係する資料**：`tasks/`、`docs/README.md`。

### D-20261113-13 見直しのセッションは実装しない

- **内容**：W1 の 12 件と EG2 の実装は、Opus の統括セッションが Codex（まず Terra）に任せて進める。Fable の見直しのセッションは、計画と資料を整えるところまでとする。Terra から始める点は D-20261113-15 で変更。
- **理由**：D-20261112-01 の役割分担。
- **関係する資料**：`AGENTS.md`「エージェント運用」、`tasks/v1-plan.md`。

### D-20261113-14 Docker のマウントがあっても Office の隔離を設定できるようにする

- **内容**：Linux の mountinfo に、隔離と関係しない種類のマウント（Docker のネットワーク名前空間など、root 欄が `net:[...]` の行）があっても、Office の renderer の隔離を設定できるようにする。この修正を v1 に足す（計画 §3.2 の OM）。隔離の要件そのものは変えない。
- **理由**：現在は、そのような行が 1 つでもあると mountinfo 全体を拒否し、Docker を使う普通の Linux デスクトップで Office 変換が使えない。開発中も Docker が動いている間は隔離のテストが落ち、検証の妨げになる。2026-11-13 に統括が見つけ、利用者が v1 に足すと決めた（D-20261113-10 の「利用者が決めたとき」）。
- **関係する資料**：`crates/kio-process/src/platform/linux/confinement_mounts.rs`、`1b46cbb5:tasks/v1-plan.md` §3.2。

### D-20261113-15 小規模以外の実装は最初から Sol に任せる

- **内容**：kio の実装の委譲で、計画の作業量が「小」以外の単位は、最初から Codex `gpt-6.2-sol` に任せる。「小」の単位は従来どおり `gpt-6.1-terra` から始める。
- **理由**：2026-11-13 に Terra に渡した 8 単位のうち、中・大の 5 単位は 7〜8 万 tokens で受入条件に届かないまま自分で止まり、すべて Sol に切り替えることになった。指示書に「途中で報告しない」と書いても変わらなかった。利用者が「現時点で小規模以外は Sol 6.1 に変更」と指示した。
- **関係する資料**：`AGENTS.md`「エージェント運用」（2026-11-13、利用者の指示で統括が反映した）。

### D-20261113-16 乱数による一時領域の識別は大きな書き込みだけに使う

- **内容**：D-20261031-01 の識別方式（物理的な ID と 256 bit の乱数、Unix は拡張属性、Windows は EA）は、中断後に再開・回収する大きな一時領域（原本・tree・復元・抽出の保存、台帳の backup、journal の整理。計画の S1〜S3、P0、L2、J1）だけに使う。store の小さな atomic 書き込みは、物理的な ID による今の方式のままとする。拡張属性・EA に対応しないファイルシステムでは、その大きな操作だけが理由を示して失敗する。
- **理由**：R1 の最初の実装は store のほぼすべての atomic 書き込みを乱数付きの一時領域に通し、拡張属性・EA に対応しないファイルシステム（FAT・exFAT、一部のネットワーク共有など）上の scope では kio が何も書けなくなった。2026-11-13 に利用者が範囲を大きな書き込みに限ると決めた。
- **関係する資料**：D-20261031-01、`1b46cbb5:tasks/v1-plan.md` §3.2 の R1、`docs/04-pipeline.md` §1.1。

## 2026-11-15

### D-20261115-01 Kio はソースコードのファイルを扱わない

- **内容**：Kio はソースコードのファイルを索引・抽出の対象にしない。既知のソースコードの拡張子のファイルは、unsupported として理由を示して飛ばす。同梱 Adapter の対象からコードを外し、コード用の MIME の判定と整形をなくす。Markdown の中のコードブロックやインラインコードは、Markdown の一部としてこれまでどおり扱う。
- **理由**：利用者が仕様を変えた。拡張子の判定を消すだけでは、中身がテキストのファイルは plain text として索引され続けるため、利用者が「非対応として飛ばす」を選んだ。
- **関係する資料**：`docs/07-adapter-spec.md`（同梱 Adapter の対象）、`docs/04-pipeline.md`（unit の表）、`1b46cbb5:tasks/v1-plan.md` §3.2 の NC。

### D-20261115-02 kio-eval の persona 系のコマンドと persona の資料を削除する

- **内容**：`kio-eval persona`（plan・schedule・render・materialize・scaffold・lease・attest と関連の module）、CI の `persona-w0-integration` の job、`persona-corpus/`、`tasks/persona-skill-corpus/` と、それらへの参照を削除する。
- **理由**：personaCorpus がこれらを使わなくなり、利用者が残すかどうかを Kio 側で決めるとした。v1 の受入（51 件）、benchmark・scale・qhard、kio-cli はこれらに依存しない（2026-11-13 の調査）。KISS・YAGNI により残さない。`persona-corpus/` は Git では骨組みだけを管理し、確定済みの成果物を含まない。
- **関係する資料**：D-20261113-12 の「`persona-corpus/` と `persona-skill-corpus/` は personaCorpus の見直しで扱う」はこの決定で変更。`1b46cbb5:tasks/v1-plan.md` §3.2 の PX。

### D-20261115-03 Kio は personaCorpus の corpus を読むだけにする

- **内容**：Kio は personaCorpus の corpus を評価の入力として読むだけで、personaCorpus への要望は出さない。
- **理由**：利用者の決定。
- **関係する資料**：`tasks/v1-plan.md` §6（評価用の corpus の扱い）。

## 2026-11-19

### D-20261119-01 古いソースコードのデータの削除、走査のメモリの有界化、Windows の CI の短縮を v1 に入れる

- **内容**：次の 3 つを v1 の残作業に足す。(1) NC より前に索引したソースコードのデータが、索引し直したときに残らないようにする（NC2）。(2) index と status の working tree の走査で、使うメモリをファイル数によらず一定にする（S2b）。(3) `step3_p0_contract` の Windows での実行時間を、検証の内容を減らさずに短くする（WP）。
- **理由**：統括は (1) を旧データの移行、(2) を仕様の有界の対象外として範囲外にしていたが、利用者が両方とも v1 で直すと決めた。(3) は残りの作業で CI を何度も回すため、利用者が v1 に入れると決めた。
- **関係する資料**：`1b46cbb5:tasks/v1-plan.md` §3.1、§3.2b。D-20261113-10（v1 の範囲は利用者が決めたときだけ足す）。

## 2026-11-20

### D-20261120-01 EG2 の runtime library は build の時に固定 URL から取得する

- **内容**：EG2 の runtime（LiteRT-LM の C API の共有 library。版は D-20261120-02。Linux x64・macOS arm64・Windows x64）は、repo に binary を commit しない。公式の release の固定 URL と target ごとの SHA-256 の一覧だけを repo に置き、CI と手元の build は取得して hash を照合してから使う。推測した path、PATH、`.kio`、current directory からは探索しない（`docs/10-operations.md` §5.1）。
- **理由**：利用者が選んだ。repo を大きくせず、版と内容は hash で固定できる。代わりに build の時にネットワークが要る。
- **関係する資料**：`docs/10-operations.md` §5.1、`1b46cbb5:tasks/v1-plan.md` §3.4 の G1。

### D-20261120-02 EG2 の runtime の版を LiteRT-LM v0.19.1（C API 1.0.0）に変える

- **内容**：EG2 の runtime の固定を、LiteRT-LM 0.18.0 から v0.19.1 の C API 1.0.0（公式 release の `litert_lm_c_api-1.0.0.zip`、SHA-256 `1d430d80eaf536f696a4922270ba55eb3d112189ac198c3066d487a45e3b4d4c`）に変える。同梱の置き場所は `lib/litert-lm/0.19.1/<target>/`。
- **理由**：0.18.0 の release には、Linux・Windows 向けのビルド済みの C API の library がない（Apple 向けの xcframework だけ）。v0.19.1 の C API 1.0.0 の zip には Linux x64・macOS arm64・Windows x64 の共有 library と `embedding_engine.h` があり、release の記載の hash と一致した（2026-11-20、統括が確認）。自前で 3 OS 向けに build するより単純である。利用者が選んだ。C API 1.0.0 は 0.18.0 と互換のない変更を含むが、EG2 の実装はまだない。
- **関係する資料**：`docs/10-operations.md` §5.1、`docs/03-data-model.md` の embedding の producer の例、D-20261120-01、`1b46cbb5:tasks/v1-plan.md` §3.4 の G1。

### D-20261120-03 Linux 固有コードの行数の上限

- **内容**：F1 で Linux 固有コードの上限を 4,000 行から 4,500 行に一度だけ引き上げる。
- **理由**：EG2 完了時の実測が 4,372 行だったため。利用者が 2026-11-20 に決定した。
- **関係する資料**：`docs/09-mvp-scope.md` §3、`crates/kio-ci/src/code_limits.rs`、`1b46cbb5:tasks/v1-plan.md` §3.5 の F1。

### D-20261120-04 F2 で見つかった仕様と実装の食い違いの扱い

- **内容**：(1) `kio diff` の派生物だけの変化の表示（QB39。現行契約は [CLI 仕様](06-cli-spec.md) §1、旧ケースの原文は `1b46cbb5:tasks/step4b-contract-tests-p3b.md`）を v1 で実装する。(2) tree と Evidence Pointer の名前は、論理名と物理名を分けて判定する。論理名は `/`・NUL・空・単独の `.`／`..` を拒否し、それ以外は履歴に保存できる。宛先 OS で安全に作れない名前は、ファイルとして書き出すときに拒否する。Evidence Pointer の `path_at_commit` も同じ論理名の規則で検証する。(3) 明示的に選んだ HTTP embedding が未承認のときは、`docs/05-runtime.md` §1.1 のとおり auto／hybrid を全文検索に縮退させ、EG2 に切り替えない。(4) 有料 OCR の Batch の提供元（Mistral OCR）は incremental の不適用を示す制御応答（`fallback_to_full`）を返さない。Batch の制御応答の記帳と Full の開始の規定を仕様から削除し、実装しない。
- **理由**：利用者の決定。(1) は承認済みの契約で、実装が欠けていた。(4) は当初 v1 で実装すると決めたが、Mistral は送られたページを常に OCR し、この応答を返す仕組みがないため、製品で到達しない経路になると分かり、利用者が仕様を変えると決め直した。(2) は Unix で正当なファイル名を保存できるようにするため、(3) は利用者が選んだ送信先を黙って別のモデルに替えないため、仕様を揃える。
- **関係する資料**：`docs/06-cli-spec.md` §1、`docs/03-data-model.md` §2・§3、`docs/08-evidence-pointer-spec.md` §2、`docs/05-runtime.md` §1.1、`docs/07-adapter-spec.md` §2.1、`docs/04-pipeline.md` §5.8、`1b46cbb5:tasks/v1-plan.md` §3.2b の H1・H2。

## 2026-11-21

### D-20261121-01 作業場所を Mac に移す

- **内容**：kio の作業場所を、個人デスクトップ WSL から Mac の `~/dev/github.com/ttokunaga-ja/kio` に移す。WSL の clone と `scratch/` は消さず、利用者が指示したら `git pull` で WSL に戻れる状態を保つ。同時に両方の環境で作業しない。役割とモデルの分担（D-20261112-01 の後半）は変えない。
- **理由**：利用者の指示。再び WSL に戻る可能性がある。
- **関係する資料**：`AGENTS.md`「エージェント運用」の「作業場所」、`tasks/handoff.md`（移管の時点の状態と再開の手順）。D-20261112-01 の作業場所の部分を置き換える。

### D-20261121-02 作業環境は時期で切り替え、作業内容では使い分けない

- **内容**：kio の作業環境は、時期によって丸ごと切り替える。通常は Mac で作業し、学会などで Mac を安定して使えない期間は個人デスクトップ WSL で作業する。切り替えは利用者が指示する。作業内容（Linux 固有の変更、Windows の動作など）によって環境を使い分けない。今の環境で確かめられない OS 固有の動作は、通常の CI で確かめる。同時に両方の環境で作業しない。切り替えるときは `tasks/handoff.md` の「環境の切り替え」の手順で、Git に入らない作業用のファイル（`scratch/` の指示書・報告・作業記録・script、`.env`）も移す。
- **理由**：利用者の指示。D-20261121-01 の「WSL に戻る」を、一時的な例外ではなく時期による切り替えとして定める。
- **関係する資料**：`AGENTS.md`「エージェント運用」の「作業場所」、`tasks/handoff.md`。D-20261121-01 を補う。

### D-20261121-03 Fable のセッションを使わず、見直しも統括（Opus）が受け持つ

- **内容**：kio の運用に Claude Fable のセッションは使わない。方針・計画・仕様・運用規則の見直しと修正も、統括（Claude Opus 5.5）が受け持ち、決めたことはこのファイルに記録する。実装・検証の Codex の分担は変えない。
- **理由**：Fable を使えなくなったため（利用者の指示）。
- **関係する資料**：`AGENTS.md`「エージェント運用」。D-20261112-01 と D-20261113-* のうち Fable を見直しの担当とした部分を置き換える。

### D-20261121-04 リポジトリの構成を整理し、正式受入は整理後に行う

- **内容**：人が付ける新しいパス名は英小文字とハイフンで統一する。日本語と日付は本文に置き、文書の通し番号は製品仕様の 01〜13 に限る。ルートの慣例名、ツール既定名、コードの識別子、契約に含まれる固定入力名は維持する。
- **構成**：`docs/` に仕様・決定と `proposals/`、`tasks/` に計画・引き継ぎ・正式受入と `acceptance/`・`agent/`・`post-v1/` を置く。固定評価入力は `eval/`、OCR 実験は `experiments/`、実行ツールと GPU 設定は `scripts/`、製品と検証の Rust コードは `crates/` に置く。`.git/` は Git の管理領域とし、`target/` は環境ごとの生成先とする。
- **Git 管理**：コード・仕様・現行手順・雛形・固定入力は Git で管理する。資格情報、実行依存、未解決の作業記録や再取得不能な証跡は環境ごとに扱う。役目を終えた契約と再生成できる生出力は現行ツリーから外し、必要な経緯は Git 履歴でたどる。`scratch/` は作業名別の指示・報告・ログ・検証・worktree と、ツール別の runtime に分ける。必要な判断と証跡を記録した後、使い終えた一時物を片付ける。
- **受入**：整理の A・B・C・D をすべて終えた構成で F4 の 51 件と最終集約を一度だけ回す。候補は整理後の通常 CI が成功した main の SHA に固定する。
- **理由**：利用者の決定。現役の仕様・入力・手順を発見しやすくし、再生成物や旧資料への依存と、候補変更による正式受入の再実行を避ける。
- **関係する資料**：[製品仕様の索引](README.md)、[正式受入](../tasks/v1-acceptance.md)、[引き継ぎ](../tasks/handoff.md)、[AGENTS.md](../AGENTS.md)「エージェント運用」。環境切替時に手で移す作業ファイルの範囲は D-20261121-02 を更新する。

### D-20261121-05 F4 の GPU 受入を 2 台の WSL に同一構成で置く

- **決定**：研究室 RTX 4060（`rm2c` / `Ubuntu-26.04`）と個人 RTX 4070（`ttokunaga-ja` / `Ubuntu` 24.04）で、WSL 内の kio-test 専用 rootless Docker Engine と NVIDIA Container Toolkit の CDI、同一候補の dispatcher・固定 OCR 入力を使う。Mac 作業中は 4060 を優先し、停止・利用不可時は 4070。4070 で作業中は 4070 を選ぶ。
- **切り替え**：受入専用鍵を両台の Windows と WSL に登録し、両台の 2 段分のホスト鍵を固定別名で pin する。受入 run の前に Environment の `KIO_LAB_JUMP_HOST` だけを手動変更し、3 OS の実行とリース回収まで固定する。
- **理由**：研究室 Docker Desktop は競馬のサービスが使用中。個人 WSL の既存 dockerd は eduanima が利用している。受入には専用ソケットと data-root を設け、共有 dockerd の設定とサービス状態を維持する。
- **関係する資料**：[正式受入](../tasks/v1-acceptance.md)、[受入ホストの構築と切り替え](../scripts/v1-local-gpu/README.md#受入ホストの構築)。
- **TCP 22 の到達制御**：Windows Firewall の限定規則と既定の受信拒否を、LAN など tailnet 外からの到達に対する多層防御とする。tailnet 内の送信元の制限は Tailscale の ACL・grant が担う。Windows 構築時は、Tailscale が生成する条件に合う `Tailscale-In` だけを広い受信許可の事前検査から除外する。許可対象外の tailnet 端末からの TCP 22 の拒否は grant の実機検証とする。
- **共有 GPU と Windows 権限**：研究室は Docker Desktop WSL integration のソケット、個人機は native `/run/docker.sock` に管理者の件数照会を向ける。接続できなければ受入を止める。研究室の Ubuntu-26.04 では、interop を無効にすると Docker Desktop の integration（`/run/docker.sock`、`/Docker/host`）が戻らないことを 2026-11-21 に実機で確かめたため、interop は有効のまま `--desktop-interop-required` で構築し、受入専用サービスの mount namespace で制限する。個人機では通常の interop を維持し、受入専用 SSH と rootless Docker に同じ制限を適用する。通常の管理者プロセス、追加のマウントや代理ソケット、WSL とカーネル実装に由来するリスクは残る。研究室の Docker Desktop を支える対話セッションを維持し、S4U はその状態で検証する。

## 2026-11-22

### D-20261122-01 Windows Office の program 配下で署名なしの PE image を許容する

- **内容**：`program/soffice.exe`、`program/soffice.bin`、および `program/` 直下のすべての PE image に有効な Authenticode 署名を要求する。`program/` の下位ディレクトリの PE image は、署名なしと確認できた場合に限り署名なしで許容する。署名の破損や信頼エラーは拒否する。署名なしを許容する範囲はこの下位ディレクトリに限る。
- **理由**：利用者の決定。固定した公式 LibreOffice 26.8.0.3 の Windows 配布物で、`program/intl/fbintl.dll` と Python の pip・setuptools 用 launcher stub 14 個に署名がなかった（CI run 44966483976 の調査）。いずれも Windows が既定で DLL を探す実行ファイルのディレクトリ `program/` の直下にない。[Windows の DLL 検索順序](https://learn.microsoft.com/en-us/windows/win32/dlls/dynamic-link-library-search-order)
- **保護と残るリスク**：実行ファイルの digest と配置全体の identity snapshot、変換直前の再確認、reparse point と複数 hard link の拒否、AppContainer、ローカル情報による信頼判定を維持する。publisher 名による許可リストは設けない。署名なしで許容した PE image の配布元や snapshot 採取前の内容は署名で保証できず、Office が明示したパスから読み込む場合もある。
- **関係する資料**：[Adapter 仕様 §5.1](07-adapter-spec.md#51-prepare)、`crates/kio-adapter/src/platform/windows/office.rs`。

### D-20261122-02 Windows Office renderer を Job で制限する

- **内容**：D-20261122-01 の AppContainer に関する判断を更新する。利用者の承認により、Windows Office の readiness probe、version 照会、実変換は AppContainer を使わず、専用 Job で起動する。Linux・macOS とその他の Windows 呼出し元には既存の隔離を適用する。
- **理由**：固定版 LibreOffice 26.8.0.3 は単一インスタンス IPC の名前を `\\.\pipe\OSL_PIPE_…` に固定しており、AppContainer 向けの `\\.\pipe\LOCAL\` を使わない。IPC を開始できないと 10 ms 間隔で再試行を続ける。run 45013605420 では親子が profile 初期化後も PDF を生成せず締切まで残った。run 45015126905 では 20 秒と 40 秒の本体ダンプの同一スレッドで 10 ms の相対待機を確認した。［[LibreOffice のパイプ実装](https://github.com/LibreOffice/core/blob/libreoffice-26.8.0.3/sal/osl/w32/pipe.cxx#L31-L35)・[IPC の再試行](https://github.com/LibreOffice/core/blob/libreoffice-26.8.0.3/desktop/source/app/officeipcthread.cxx#L702-L734)］
- **保護と残るリスク**：プロセス数・メモリ・CPU 時間・Job close 時の終了、`CREATE_NO_WINDOW`、時間と出力を制限した I/O、呼出しごとの private profile と scratch、実行ファイルの同一性・digest・D-20261122-01 の署名検証を維持する。Office は呼出し元ユーザーの権限でファイル、レジストリ、IPC、ネットワークにアクセスできる。Job はそれらのアクセスを制限しない。
- **v1 後の対応**：許可を限定したホスト側のパイプ仲介、または LibreOffice 上流での AppContainer 対応を検討し、AppContainer を復元する。
- **関係する資料**：[Adapter 仕様 §5.1](07-adapter-spec.md#51-prepare)、[運用 §4.3](10-operations.md#43-kio-created-object-boundary)。

### D-20261126-01 F4 の後継キャンペーンと 6 レーンの再実行を承認

- **内容**：保護された台帳に後継キャンペーンを追記し、F4 の有料 6 レーンを 1 回再実行する。先行キャンペーンの予約をすべて引き継ぎ、上限は各プロバイダー累計 USD 10 のままとする。
- **理由**：Windows Git Bash が GitHub API の引数をファイルパスへ変換し、先行実行が失敗した。修正後のワークフローのハッシュを後継キャンペーンに結び付ける。
- **関係する資料**：`tasks/acceptance/provider-budget.md`。

### D-20261127-01 有料プロバイダー受入のワークフロー SHA-256 を来歴に変更

- **内容**：利用者の承認により、予約・検証・エクスポートからキャンペーンのワークフロー SHA-256 との一致条件を外す。新規予約に実行時のワークフロー SHA-256 を記録する。後継キャンペーンからの遷移を認め、全履歴での ID の一意性と親の連続性を維持する。台帳の履歴上限を 256 コミットとする。
- **理由**：ワークフロー変更後も既存の `v1-f4-2026-11b` を使えるようにする。D-20261126-01 のハッシュ束縛に関する判断を更新する。
- **保護**：main 限定の dispatch、専用 GitHub App による台帳更新、レーン単位の予約、全履歴を通算したプロバイダーごとの USD 10 上限、払い戻しなしを維持する。
- **関係する資料**：`tasks/acceptance/provider-budget.md`。

### D-20261128-01 Codex の reasoning effort の既定を low に変更

- **内容**：利用者の指示により、Codex に委譲するときの reasoning effort の既定を medium から low に変える。難しい調査・設計・レビューでは、統括がその作業に限って上げる。
- **理由**：Codex の利用量を抑える。kio 以外の repo と `~/.codex` の既定も同じく low にする。
- **関係する資料**：`AGENTS.md`（エージェント運用）、`tasks/handoff.md` §4。

### D-20261129-01 外部送信を一括の許可にし、loopback の承認と Office の隔離を見直す

- **内容**：利用者の決定（2026-11-29）。
  1. 外部 Adapter への送信は、Adapter・宛先・出力 profile ごとの承認ではなく、外部送信の許可 1 つで扱う。許可すれば設定した外部 Adapter をすべて使え、許可しなければすべて使わない。
  2. loopback（127.0.0.1 など）の HTTP embedding への送信は承認を要しない。
  3. Linux・macOS で LibreOffice の隔離を作れない環境でも、利用者が設定で明示した場合は隔離なしで Office を変換する。明示がなければ現状どおり変換しない。
  4. provider 受入の Environment と専用 App を統合する。
- **理由**：Adapter ごとの承認は利用者の認知負荷を上げるだけで、利用者が想定するセキュリティ要件は外部送信の可否で満たせる。利用者が求めていない細かすぎる設定もあわせて簡潔にするため、調査する。
- **関係する資料**：[F4 後の作業計画](../tasks/post-f4-plan.md) P5、`docs/07-adapter-spec.md`、`tasks/acceptance/provider-budget.md`。
