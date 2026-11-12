# v1 の計画

この文書が、kio の計画のただ一つの入口である。利用者の決定は [docs/decisions.md](../docs/decisions.md)、製品の仕様は `docs/01`〜`docs/14`、役割・モデル・停止の基準は [AGENTS.md](../AGENTS.md) が正本で、ここには写さない。作業が進んだら、この文書の「現状」と「残作業」を書き換える。経過の記録は commit に残し、ここには積まない。

## 1. 現状（2026-11-13、main `56a6831`）

- v1 の基本機能（保存、検索、履歴と復元、承認、変更の監視と自動管理、復旧、3 つの OS への対応）は実装済みである。
- 構造の是正（OS 固有層の分離、`kio-app` の分解と型付き API、巨大ファイルの分割）と、embedding の経路の統一（O 段階）は main に入っている。
- 2026-10-22 に承認された制約の緩和（W1）は、62 件のうち 9 件が main に入っている（`b0906e3`、`652e6cb`、`8ff79bf`）。残りは 53 件で、一覧は [w1-constraints.tsv](w1-constraints.tsv) にある。
- main は WSL でのローカル検証（fmt、strict clippy、全テスト 3,835 件）と、通常の CI の 3 つの OS で成功している（`22a0c73`）。
- 共通コードは 111,757 行で、上限の 106,000 行を超えている。CI はこの超過を報告だけして通す設定になっている（`.github/workflows/ci.yml` の code-limits の step）。

## 2. 完了条件

二段に分ける（D-20261113-02）。

1. **実装完了**：3 の残作業がすべて終わり、main で 4.1 のローカル検証と通常の CI の 3 つの OS が成功している。
2. **正式な受入**：実装完了の commit を候補として固定し、51 件（3 つの OS のネイティブ 42 件、有料 provider 6 件、GPU 経由のローカル 3 件）と最終の集約を一度だけ回す。51 件は減らさない。

実装完了までは、正式な受入の workflow（`v1-acceptance*.yml`、`v1-provider-acceptance.yml`、`v1-local-acceptance.yml`）を回さない。

## 3. 残作業

上から順に進める。番号の付いた行が、Codex（まず Terra）に渡す 1 回分の作業である。受入条件の詳細は、挙げた仕様書の節に従う。

### 3.1 検証の土台

| 番号 | 作業 | 受入条件 |
|---|---|---|
| V1 | テストが実際のホームに書き込まないようにする。`XDG_DATA_HOME` だけを差し替えて `XDG_STATE_HOME` や `HOME` を差し替えていないテストが 47 ファイルある | 全テストの実行の前後で、`~/.local/share/kio`、`~/.local/state/kio`、`~/.config/kio`、`~/.cache/kio` に変化がない |

### 3.2 制約の緩和の残り（W1 の優先度「高」12 件）

**範囲は利用者の判断待ち**（高の 12 件を v1 に入れ、中・低の 41 件を v1 後に回す案）。仕様書は `6656312` で緩和後の内容に書き換え済みである。

| 番号 | 対象（[w1-constraints.tsv](w1-constraints.tsv) の ID） | 内容 |
|---|---|---|
| W1 | ST05、ST04、ST06 | 直下 1 万ファイル、原本 512MiB・scope 合計 4GiB、復元の 64MiB・合計 512MiB の固定上限を、警告・逐次処理・資源の段階に置き換える |
| W2 | AD-14、AD-16 | PDF と XLSX の固定上限を同じ形に置き換える |
| W3 | AD-29、AD-30 | 台帳の snapshot・backup と、task・unsupported の journal の固定上限を置き換える |
| W4 | APP-26 | 前処理の結果が変わったときに索引と watch を止めず、新しい世代として保存する |
| W5 | APP-11、CLI-01 | 自分の watch service の更新を許す。scope の中の新しい path への export を許す |
| W6 | AD-17、PROC-01 | Office の renderer を実際の能力と配置で判定する。Linux で隔離の部品が足りないときに、足りない部品と別の抽出経路を示す |

### 3.3 ローカル接続の汎用化（L1 の残り）

| 番号 | 作業 | 受入条件 |
|---|---|---|
| L1 | ローカルの embedding 接続で、OpenAI 互換の input 形式と messages 形式を選べるようにする。loopback の HTTP と端末ごとの承認は実装済み（`8ff79bf`） | `docs/07-adapter-spec.md` のローカル接続の節。形式を明示して選ぶ、同じ profile で混ぜない、text だけの接続に画像を送らず理由付きで飛ばす、出力は 768 次元 |

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
| F1 | 共通コードの上限を、EG2 までの実測から一度だけ引き上げ、CI の code-limits を必須にする（2026-10-18 承認） | `kio-code-limits` が違反なしで終わり、CI が終了値 1 を通さない |
| F2 | 仕様書の整理。`docs/14` を対応する仕様書へ統合して削除し、実装の内部の手順と日付付きの経緯を除く（2026-10-06 承認） | 仕様書どうし、仕様書と実装の間に食い違いがない。リンク切れがない |
| F3 | 実装完了の判定 | 2 の「実装完了」 |
| F4 | 正式な受入 | 2 の「正式な受入」 |

## 4. 検証の回し方

### 4.1 ローカル（WSL）

CI の Linux の job と同じ順で回す。Codex のサンドボックスの中ではテストが通らない（`/` と `/home` の所有者が別の UID に見える）ので、全テストと synthetic-history は統括がサンドボックスの外で実行する。

```bash
cargo +1.98.0 fmt --all -- --check
cargo +1.98.0 clippy --workspace --all-targets --locked -- -D warnings
systemd-run --user --scope -p Delegate=yes \
  cargo +1.98.0 test --workspace --all-targets --locked --no-fail-fast
```

- 作業中は、変更した crate のテストだけを回す。全テストは、作業の区切りと push の前に回す。
- persona W0 と synthetic-history は、`kio-eval`・検索・索引・履歴に触れた変更の push の前に回す。手順は `.github/workflows/ci.yml` の該当 job のとおり。
- V1 が終わるまでは、全テストの後に `~/.local/state/kio` などへ書き込まれていないかを確かめる。

### 4.2 CI

push すると通常の CI が動き、50〜100 分かかる。push は、ローカル検証が通った区切りでまとめて行う。macOS と Windows の結果はここでしか分からないので、OS 固有の変更は小さい単位で push する。

### 4.3 高いコストの検証

正式な受入、Full 規模の性能計測、GPU を使う計測、有料 provider を使う計測は、実装完了まで回さない。手順は [manual-full-cold-gates.md](manual-full-cold-gates.md)。

## 5. デスクトップ単独では回せないもの

- macOS と Windows のネイティブの検証：通常の CI で確かめる。
- 有料 provider の 6 件と GPU 経由の 3 件：正式な受入のときに、外部の資源（provider の鍵と予算、Tailscale、SSH、GPU の host）の準備を確かめてから回す。過去の設定の記録から、現在使えるとは判断しない。

## 6. v1 後

- 文書単位の検索結果（D-20261113-01、[search-result-presentation-design.md](search-result-presentation-design.md)）。
- 版ごとの状態の表示、tag による引用の保持、復元の操作の表示（2026-10-03 の方針）。
- 制約の緩和の中・低の 41 件。
- personaCorpus の corpus を使った品質・性能の評価。
