# U7 — serving 経路の image/text 同一空間 受け入れ検査

`tasks/local-adapter-plan.md (git 56a6831)` の **U7**。ある serving 経路を採用してよいかを、
**参照実装 (PyTorch) との数値一致**で決める。

## この検査が存在する理由

llama.cpp Discussion #14851 (jina-embeddings-v4) の実測:

> text embeddings matched perfectly, **image embeddings diverged significantly**

**Kio の互換ゲートはこれを原理的に検知できない。** 次元も distance metric も
modality も `profile_hash` も、すべて一致するからである
([03 §7](../../docs/03-data-model.md))。しかも embedding は content-addressed
identity を持ち first-instance-wins で永続化されるので、**誤った空間の画像ベクトルが
恒久的に凍結される**。気付いた時点で再埋め込みしか手が無い。

同じ意味的 profile で別の serving runtime を採る候補を、text と image の数値一致で評価する。

## 使い方 (GPU 実機)

```bash
# 比較対象のローカル messages endpoint を起動しておく。

kio-eval u7 \
  --base-url http://127.0.0.1:8000 \
  --model "my-embedding-model" \
  --reference-python /absolute/path/to/the/reference/venv/bin/python3 \
  --reference-adapter /absolute/path/to/kio/eval/u7/reference_adapter.py \
  --reference-model /absolute/path/to/pinned-local-reference-model \
  --text "same-space text control" \
  --image /absolute/path/to/control-image.png \
  --out /absolute/path/to/u7-same-space-report.json
```

`kio-eval u7` が HTTP wire、comparison、verdict、および report を所有する。参照側だけは
`torch` と `transformers` を使う JSONL adapter で、HTTP、判定、report、filesystem探索を
持たない。interpreter は暗黙の `PATH` 探索をせず、依存を導入した venv の絶対 path を
`--reference-python` で明示する。reference modelもremote repository IDではなく、
事前にreviewしたlocal directoryのcanonical absolute pathを明示する。adapterは
`local_files_only`でロードする。Rust は child environment を clear し、cache/GPU の
非credential設定だけを allowlist する。
text control は最低 1 件必須で、画像は `--image` を繰り返して明示する。追跡済みの
`experiments/ocr-verification/fixtures/images` を使えば追加費用はないが、
Rust runner が暗黙に directory 探索することはない。report は create-only である。

## 結論の読み方 — text は対照群である

| `reason` | 意味 | 次にすること |
|---|---|---|
| `both-agree` | text も image も一致 | **採用してよい** |
| `image-diverged` | text は一致・image が乖離 | **これが探している欠陥。採用しない。**既に埋めた分は再埋め込み |
| `harness-suspect` | **text が一致しない** | serving 経路の判定ではない。**この計器の側を疑う** |
| `image-not-measured` | image を 1 件も測っていない | 判定は未完了 |

`harness-suspect` を独立した結論にしてあるのが要点である。報告されている失敗は
「text は合う・image だけずれる」なので、**text すら合わないなら、それは経路ではなく
参照ハーネスの描画・pooling・正規化が serving 側と揃っていない疑いが濃い**。
ここを取り違えると、健全な経路を捨てるか、壊れた経路を通すことになる。

**`harness-suspect` のとき image の数字を読まないこと。**

## 合算しない

判定はモダリティごとに、しかも**最小値**で行う。平均を採ると探している欠陥
(片方だけずれる) を計器が自分で消す。この性質は Rust の vector test が固定しており、
合算する実装に変えると落ちる。モダリティ内で最小を採るのも同じ理由で、1 枚でも
別空間なら**その 1 枚のベクトルが凍結される**。

## 閾値について

既定は `0.999`。これは**実測された境界ではない**。U2 は同一ランタイム内の参照計算に
対し cos 0.999999994 (最大絶対差 7e-9 = f32 の丸め) を実測しており、ランタイムを
またぐ本検査はカーネル差でそこまでは寄らない。一方で捕まえたいのは
「significantly diverged」と表現された水準である。0.999 はその間に置いた既定値で、
`--threshold` で動かせる。

> **0.99〜0.999 に着地したら「惜しい」ではなく裁定対象である。** 参照ハーネスの
> 不一致なのか経路の欠陥なのかを切り分けるまで、採用の可否を決めないこと。

## MRL 切り詰めは判定に使わない

Kio は 768 へ切り詰めて再正規化するが、本検査は native 次元のまま比較する。
切り詰めは決定的な後処理であって、**空間が違うことを直せないし作りもしない**。

## 適用範囲

この検査は利用者が設定するローカル endpoint 向けである。EG2 の確認手順は
[CLI と runtime の仕様](../../docs/07-adapter-spec.md#embeddinggemma-2-eg2既定-local) を参照する。
