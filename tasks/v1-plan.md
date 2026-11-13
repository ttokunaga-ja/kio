# v1 の計画

この文書が、kio の計画のただ一つの入口である。利用者の決定は [docs/decisions.md](../docs/decisions.md)、製品の仕様は `docs/01`〜`docs/14`、役割・モデル・停止の基準は [AGENTS.md](../AGENTS.md) が正本で、ここには写さない。作業が進んだら、この文書の「現状」と「残作業」を書き換える。経過の記録は commit に残し、ここには積まない。

## 1. 現状（2026-11-13）

- v1 の基本機能（保存、検索、履歴と復元、承認、変更の監視と自動管理、復旧、3 つの OS への対応）は実装済みである。
- 構造の是正（OS 固有層の分離、`kio-app` の分解と型付き API、巨大ファイルの分割）と、embedding の経路の統一（O 段階）は main に入っている。
- 2026-10-22 に承認された制約の緩和は、62 件のうち 9 件が main に入っている（`b0906e3`、`652e6cb`、`8ff79bf`）。残り 53 件のうち、優先度が高い 12 件を v1 に入れる（D-20261113-09）。一覧は [w1-constraints.tsv](w1-constraints.tsv)。
- 3.1 の V1・V2 と、3.2 の R0・R1・S1・L1・J1・A1・E1・O0・OM は main に入っている（2026-11-13）。この commit は WSL のローカル検証（4.1 の全テスト、persona W0、synthetic-history）で成功している。通常の CI の 3 つの OS で最後に成功を確かめた commit は `9c7fb32` である。
- 共通コードは 120,714 行で、上限の 106,000 行を超えている（2026-11-13 の実測。3.5 の F1）。CI はこの超過を報告だけして通す設定になっている。
- v1 の範囲は 3 の残作業で固定されている。足すのは利用者が決めたときだけで、それ以外は 6 の「v1 後」に書く（D-20261113-10）。

## 2. 完了条件

二段に分ける（D-20261113-02）。

1. **実装完了**：3 の残作業がすべて終わり、main で 4.1 のローカル検証と通常の CI の 3 つの OS が成功している。
2. **正式な受入**：実装完了の commit を候補として固定し、51 件（3 つの OS のネイティブ 42 件、有料 provider 6 件、GPU 経由のローカル 3 件）と最終の集約を一度だけ回す。51 件は減らさない。

実装完了までは、正式な受入の workflow（`v1-acceptance*.yml`、`v1-provider-acceptance.yml`、`v1-local-acceptance.yml`）を回さない。

## 3. 残作業

上から順に進める。番号の付いた行が、Codex に渡す 1 回分の作業である（作業量が「小」の行は Terra、それ以外は Sol。AGENTS.md）。受入条件の詳細は、挙げた仕様書の節に従う。

### 3.1 検証の土台

V1（テストが実際のホームに書き込まない）と V2（Linux の全テストの job を 25 分以内にする）は完了した。V2 は `ci2_replay_history` の削除と、CI の 3 つの OS のテストを test profile の opt-level 1 で build することで、Linux の job が 19.5 分になった（`9c7fb32`）。Windows の remaining の job は 82.5 分かかり（`step3_p0_contract` が Windows だけで 21 分）、制限を 120 分にしてある。この 1 本を Windows で軽くするのは未着手である。

### 3.2 制約の緩和の残り（優先度「高」12 件）

12 件は、共通の仕組み 3 つを含めて 18 の作業単位に分ける。これとは別に、利用者の決定で OM（D-20261113-14）を足した。見積もりは、Codex の実装・修正が合計 28〜42 回、1 回を半日とすると 14〜21 日である（統括の確認、CI の待ち時間、環境の問題への対応は含まない）。静的な調査にもとづく幅のある数字で、単位ごとの根拠・受入条件・変更してよい範囲・過去の試作でつまずいた点は [w1-high-estimate.md](w1-high-estimate.md) にある。共通コードは、さらに 2,500〜8,500 行増える見込みである。

作業量は、小＝Codex 1 回、中＝1〜2 回、大＝3 回以上または設計の判断が要る、を表す。「3 OS」の列が「必須」の単位は、その単位だけで push して CI で確かめる。

| 番号 | 範囲 | 先に済ませる単位 | 作業量 | 3 OS |
|---|---|---|---|---|
| S2 | ST05：直下 1 万ファイルの上限を警告に変える | R0、R1、S1 | 大 | 必須 |
| S3 | ST06：復元の 64MiB・合計 512MiB の上限をなくす | S1、R1 | 大 | 必須 |
| P0 | 抽出した結果を一時領域から公開する共通の経路と、資源不足のときの保留。後段の 256MiB の上限も同じ系列で直す | R0、R1、下の判断 2・6 | 大 | 必須 |
| P1 | AD-14：PDF の page・object・展開の上限を資源の段階に置き換える | P0 | 中 | P2 とまとめて |
| P2 | AD-16：XLSX の sheet・row・cell の上限を資源の段階に置き換える | P0 | 中 | P1 とまとめて |
| W1 | APP-11：自分の watch service の更新を許す | O1 | AD-17：Office の renderer を、隔離の中での実際の変換で判定する | O0、A1、OM | 大 | 必須（実 renderer） |
| O2 | AD-17：Linux で、検証できる配置の runtime を受け入れる | O1 | 中 | Linux 必須 |
| O3 | AD-17：macOS と Windows で、販売元の固定をやめ、有効な署名と実際のコードの対応で判定する | O1 | 大 | 必須 |

進める順番：R0、R1、S1、L1、J1、A1、E1、O0、OM は完了した。W1 を進めている。次に R1 を固め、S1〜S3、P0〜P2、L1・L2、J1、W1 へ進む。O1〜O3 は A1 の後に進める。

**先に決めること**（統括が、現在の仕様書とコードで確かめて決める。利用者の判断が要るものは、決める前に確認する）

1. **中断後の一時領域の識別方法**（R1 で実装済み。範囲は D-20261113-16）。2026-10-31 に、物理的な ID と 256 bit の乱数を、Unix は拡張属性、Windows は EA に書く方式が選ばれている（D-20261031-01）。現在の main は物理的な ID だけで識別している。この方式を R1 で実装する。対応していないファイルシステムでの扱いが仕様書に書かれていなければ、利用者に確認する。
2. **無料の前処理が資源不足で保留になった後の再開**。機械の状態による不足は次の watch で自動的に確かめ直し、入力による不足は明示的な再開を待つ、という 10 月の監督の判断と、`docs/04-pipeline.md` の「同じ条件の watch では再実行しない」の関係を揃える。
3. **台帳の backup が元の台帳を変えないこと**。仕様は元の台帳の bytes と権限を変えないと定めているが、現在の実装は backup の前に WAL を切り詰めている。仕様に合わせ、元を変えない方式にする。
4. **APP-26 の由来の記録**。仕様は必須としているが、現在の manifest にはない。確認の除去だけを先に入れず、記録の読み書きまでを A1 に含める。
5. **journal の整理の方式**。現在の状態だけを原子的に書き直す方式を第一候補とし、分割の管理は先回りして作らない。
6. **抽出の資源の段階と、既存の入力サイズの制限の関係**。原本のサイズで飛ばす現在の経路が、段階を上げても手前で止める恐れがある。原本の保存、ローカルの抽出の予算、外部の Adapter への入力の制限を分けて整理する。
7. **Office の対応範囲**。検証できる runtime の配置だけを受け入れ、分からない配置は理由を示して使えないものとする。systemd と cgroup を必須とすること、段階の数値は仕様書で決まっており、決め直さない。

### 3.3 ローカル接続の汎用化

| 番号 | 作業 | 受入条件 |
|---|---|---|
| LW1 | ローカルの embedding 接続で、OpenAI 互換の input 形式と messages 形式を選べるようにする。loopback の HTTP と端末ごとの承認は実装済み（`8ff79bf`） | `docs/07-adapter-spec.md` のローカル接続の節。形式を明示して選ぶ、同じ profile で混ぜない、text だけの接続に画像を送らず理由付きで飛ばす、出力は 768 次元 |

### 3.4 既定のローカル embedding（EG2）

仕様は `docs/07-adapter-spec.md`（EG2 の節）、`docs/10-operations.md`（モデルの取得）、`docs/11-product-requirements.md`（受入の要求）。設計の経緯は repo の外の `~/archive/mac-20261112/home/Documents/Codex/2026-10-18/kio-embed-spec/runs/A1/` にあるが、その中の「認定外の Linux・CPU を拒否する」「旧ローカル窓を全部削除する」は後の決定で取り消されている。仕様書と食い違うときは仕様書に従う。

| 番号 | 作業 | 受入条件の骨子 |
|---|---|---|
| G0 | モデルと runtime の用意。runtime は配布物に同梱し、モデルは明示的な取得か import だけで入れる | 状態の表示、offline での import、破損・中断・容量不足の扱い、既存の有効なモデルの保持、暗黙のダウンロードがないこと |
| G1 | 管理された worker。同じ binary を worker として起動し、コマンドの終了時にメモリを解放する | 上限のある IPC。異常終了・EOF・不正な応答・timeout・取消しで親が生き残る。失敗した vector を公開しない |
| G2 | prompt、窓の分割、集約、profile | 上限を超えたときだけ分割する。原文を欠かさず覆う。全部の窓が成功するまで公開しない |
| G3 | EG2 を既定に切り替え、旧 Qwen 専用の設定・実装・テストを消す。汎用のローカル接続と共有の HTTP・trust は残す | 既定の経路が EG2 になる。回復と承認の回帰テストが通る |

### 3.5 仕上げ

| 番号 | 作業 | 受入条件 |
|---|---|---|
| F1 | 共通コードの上限を、EG2 までの実測から一度だけ引き上げ、CI の code-limits を必須にする（D-20261018-01）。3.2 と EG2 で行数は増えるので、途中では削減のための作業をしない | `kio-code-limits` が違反なしで終わり、CI が終了値 1 を通さない |
| F2 | 仕様書の整理。`docs/14` を対応する仕様書へ統合して削除し、実装の内部の手順と日付付きの経緯を除く（D-20261006-01） | 仕様書どうし、仕様書と実装の間に食い違いがない。リンク切れがない |
| F3 | 実装完了の判定 | 2 の「実装完了」 |
| F4 | 正式な受入 | 2 の「正式な受入」 |

## 4. 検証の回し方

### 4.1 ローカル（WSL）

CI の Linux の job と同じ順で回す。全テストは、systemd の scope の中で、HOME と XDG を repo の `scratch/` に向けて実行する。Codex のサンドボックスの中ではテストが通らない（`/` と `/home` の所有者が別の UID に見える）ので、全テストと synthetic-history は統括がサンドボックスの外で実行する。

```bash
cargo +1.98.0 fmt --all -- --check
cargo +1.98.0 clippy --workspace --all-targets --locked -- -D warnings

I="$PWD/scratch/validate"; rm -rf "$I"; mkdir -p "$I"/{home,config,data,cache,state,tmp}
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
HOME="$I/home" XDG_CONFIG_HOME="$I/config" XDG_DATA_HOME="$I/data" \
XDG_CACHE_HOME="$I/cache" XDG_STATE_HOME="$I/state" TMPDIR="$I/tmp" CARGO_PROFILE_TEST_OPT_LEVEL=1 \
systemd-run --user --scope -p Delegate=yes \
  -E HOME -E XDG_CONFIG_HOME -E XDG_DATA_HOME -E XDG_CACHE_HOME -E XDG_STATE_HOME \
  -E TMPDIR -E RUSTUP_HOME -E CARGO_HOME -E CARGO_PROFILE_TEST_OPT_LEVEL \
  cargo +1.98.0 test --workspace --all-targets --locked --no-fail-fast

ls -d ~/.local/share/kio ~/.local/state/kio ~/.config/kio ~/.cache/kio   # 何も出ないこと
```

- `CARGO_PROFILE_TEST_OPT_LEVEL=1` は CI の 3 つの OS のテストの手順と同じ設定である（debug_assertions は有効のまま）。
- 作業中は、変更した crate のテストだけを回す。全テストは、作業の区切りと push の前に回す。
- persona W0 と synthetic-history は、`kio-eval`・検索・索引・履歴に触れた変更の push の前に回す。手順は `.github/workflows/ci.yml` の該当 job のとおり。
- 最後の `ls` は、テストが実際のホームに書き込んでいないことの確認である。V1 の後は、HOME と XDG を差し替えずに全テストを回しても何も作られない（2026-11-13 に確認）。

### 4.2 CI

push すると通常の CI が動き、全体で約 85 分かかる（Windows の remaining の job が最も長い）。Linux の全テストの job は約 20 分で、制限は 45 分である。push は、ローカル検証が通った区切りでまとめて行う。macOS と Windows の結果はここでしか分からないので、OS 固有の変更は小さい単位で push する。

### 4.3 高いコストの検証

正式な受入（接続の経路は [v1-private-actions-route.md](v1-private-actions-route.md)）、Full 規模の性能計測、GPU を使う計測、有料 provider を使う計測は、実装完了まで回さない。手順は [manual-full-cold-gates.md](manual-full-cold-gates.md)。

## 5. デスクトップ単独では回せないもの

- macOS と Windows のネイティブの検証：通常の CI で確かめる。
- 有料 provider の 6 件と GPU 経由の 3 件：正式な受入のときに、外部の資源（provider の鍵と予算、Tailscale、SSH、GPU の host）の準備を確かめてから回す。過去の設定の記録から、現在使えるとは判断しない。

## 6. v1 後

- 文書単位の検索結果（D-20261113-01、[search-result-presentation-design.md](search-result-presentation-design.md)）。
- 版ごとの状態の表示、tag による引用の保持、復元の操作の表示（D-20261003-01、[knowledge-ux-implementation-plan-2026-10-03.md](knowledge-ux-implementation-plan-2026-10-03.md)）。
- 制約の緩和の中・低の 41 件（D-20261113-09。[w1-constraints.tsv](w1-constraints.tsv) の「今回の整理」が「残件候補」で、優先度が「中」「低」の行）。
- 品質・性能の評価。評価用の corpus の原本は `~/archive/mac-20261112/home/` の `kio-dogfood/`（`corpus-v1` と最新の fixture）と `kio-baseline-corpus*` にある。使うときは repo の `scratch/` に写し、`kio-eval benchmark` の `--fixture-root`・`--baseline-corpus` で指定する。v1 のテストと受入はこれらを使わない（D-20261113-11）。
