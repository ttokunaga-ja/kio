# `/layout-parsing` の生応答キャプチャ

PaddleOCR-VL が実際に返したバイト列。**整形も切り詰めもしていない。**
`curl` の出力をそのまま置いてある (compact JSON、末尾に改行なし、CR なし)。

置いてある理由は、mock がサービスに 3 回続けて食い違い (応答の封筒 / 図の綴り /
末尾 LF)、**3 回とも CI が緑のままだった**ため。mock とコードが互いに同意し、
サービスだけが両方と食い違っていた。実物があれば、その系統を閉じられる。

## 採取条件 (2026-08-03)

| | |
|---|---|
| Pipeline | `paddleocr-vl:latest-nvidia-gpu-offline` <br> `sha256:6c735bdf9e758ffdd58ccc067db0c2d84e37e5e6a2cbd47156069d4d7ea5d709` |
| VLM backend | `paddleocr-genai-vllm-server:latest-nvidia-gpu-offline` <br> `sha256:d0d32c04a2119613d25a0a4c292e165ccc107954b74580613cf59e378037f8f5` |
| 重み | `PaddleOCR-VL-1.6` / `model.safetensors` `sha256:85a479d5…71db` |
| ハード | RTX 4070 / WSL2 |

## 採取条件 (2026-08-09 / 第 5 回) — 第 4 回と一致

第 5 回の 5 本は、**上の 4 項目がすべて一致した状態**で採った。推定ではなく実測:

| | 第 4 回に記録された値 | 第 5 回の実測 |
|---|---|---|
| Pipeline digest | `sha256:6c735bdf…5d709` | **一致** |
| VLM digest | `sha256:d0d32c04…7f8f5` | **一致** |
| 重み sha256 | `85a479d5…71db` | **一致** (単一 safetensors / 1,917,255,968 B / shard 無し) |
| ハード | RTX 4070 / WSL2 | **一致** |

重みの digest はコード側の pin (`local_ocr_markdownize.rs`) とも 64 桁すべて一致する。

**`compose.yaml` と `.env` は「この機から消えた」と判断されたが、実際には残っていた。**
`~/paddleocr-vl` ではなく一時ディレクトリに置かれていたので、探し方が合っていなかった。
一時ディレクトリはいつ消えてもおかしくないため、**原本を
[`tasks/artifacts/paddleocr-vl-compose/`](../../../../../tasks/artifacts/paddleocr-vl-compose/)
へ保全した** (`compose.yaml` / `compose.override.yaml` / `backend-config.yaml` / `env.txt`)。
`docker inspect` の全文も `tasks/artifacts/paddleocr-{pipeline,vllm}-inspect.json` にある。
どちらにも資格情報は無い — `GPG_KEY` は python 公式イメージが持つ**公開**署名鍵の
フィンガープリントで、秘密ではない。

> ⚠ **この応答は上流の既定設定のものではない。**`compose.override.yaml` が
> `backend-config.yaml` を渡しており、`gpu_memory_utilization: 0.85` /
> `max_model_len: 16384` / `max_num_batched_tokens: 16384` / `api_server_count: 1`
> で動いている。12 GB のカードに載せるためのサイズ調整で、既定のままでは
> KV キャッシュが負になりエンジンが起動しない。**サンプリングにもテンプレートにも
> 重みにも触っていない**が、「上流の既定で再現する」とは書けない。

リクエストボディは **Kio の `EnvLocalOcrClient` が送るものと同一**:

```json
{"file": "<base64>", "fileType": 1, "useLayoutDetection": true}
```

`…-as-pdf.json` だけは `"fileType": 0` (PDF)。理由は下の「PDF 入力」を参照。

**可視化オプションは付けていない。** Kio が投げない要求への応答を fixture にすると、
また「サービスと違う形」を固定することになるため。なお `inputImage` と
`outputImages.layout_det_res` は**既定で応答に載ってくる**ので入っている
(これがサイズの大半を占める)。

> `base64 -w0` の結果を `-d "$(...)"` でシェル引数に渡すと ARG_MAX を超えるので、
> ボディはファイルに書いて `--data-binary @file` で送った。ワイヤ上のバイトは同じ。

## 入力

すべて**リポジトリ内の合成画像** (`experiments/ocr-verification/fixtures/images/`)。
公開済み・再現可能で、社内文書は使っていない。

| ファイル | 入力画像 | `fileType` | サイズ |
|---|---|---|---:|
| `infographic-two-charts.json` | `infographic-high.png` | 1 (image) | 1,472,174 B |
| `invoice-table.json` | `invoice-photo.png` | 1 (image) | 1,117,633 B |
| `slide-single-figure.json` | `slide-dense.png` | 1 (image) | 1,285,842 B |
| `infographic-two-charts-as-pdf.json` | 同上を 150 dpi で PDF 化 | **0 (pdf)** | 1,279,343 B |
| `code-editor-no-crops.json` | `code-editor.png` | 1 (image) | 613,462 B |
| `terminal-dark-no-crops.json` | `terminal.png` | 1 (image) | 631,733 B |
| `chat-light-avatar-crops.json` | `chat.png` | 1 (image) | 737,967 B |
| `whiteboard-no-crops.json` | `whiteboard-flow.png` | 1 (image) | 1,031,702 B |
| `seal-crop-uncited.json` | `circular-photo.png` | 1 (image) | 1,175,730 B |

## PDF 入力 (2026-08-05 追加)

`infographic-two-charts-as-pdf.json` は**同じ画像を PDF に包んで投げた応答**。
上の 3 本はすべて `fileType: 1` なので、**PDF 応答の形はどれも押さえていなかった**。

`dataInfo` の形が違う。ページ寸法が配列の中へ移り、`numPages` が増える:

```json
image: {"width":1024,"height":1536,"type":"image"}
pdf  : {"numPages":1,"pages":[{"width":984,"height":1475}],"type":"pdf"}
```

**中身も同一ではない。**PDF 経路は 150 dpi でラスタライズし直すため寸法が
1024×1536 → 984×1475 に変わり、レイアウト検出の結果もわずかにずれる:

| | image 入力 | pdf 入力 |
|---|---:|---:|
| block 数 | 48 | **49** |
| クロップ数 | 20 | **21** |
| `markdown.text` 中の `<img>` | 19 | **20** |
| `chart` | 2 | 2 |

同じ文書でも**入れ方が変われば検出数が変わる**。決定性が保証されるのは
「同じバイト列を同じ経路で投げたとき」までで、それ以上ではない。

### ずれるのは座標だけではない — 可視テキストも変わる

上の表は件数だけを並べているが、**読める文字列そのものが変わる**。
`Risk: medium` は image 経路では見出しに 1 回だけ現れるのに対し、
**pdf 経路では見出しと本文の両方に現れて 2 回になる**:

```
image: ('paragraph_title', 'Risk: medium')  ('text', 'Potential text loss in images')
pdf  : ('paragraph_title', 'Risk: medium')  ('text', 'Risk: medium\nPotential text loss\nin images')
```

別の `footer` では改行位置が変わる (image は `...detected.\nOCR output`、
pdf は `...detected. OCR output`)。

**これは chunk の中身が変わるということである。**箱がずれるだけなら
`related_images` の当たり所の話で済むが、テキストが変われば検索の対象と
埋め込みの入力が変わる。同じ 1 枚の絵でも、**PNG で入れたか PDF に包んだかで
索引の中身が違う**ものになり、しかもどちらも「正しい OCR 結果」である。

> **07 §9 の first-instance-wins は、ここでは助けにならない。**あれは同じ
> content-addressed object に対する規則であり、PNG とそれを包んだ PDF は
> raw hash が違うので**別の object になる**。したがって片方が凍結されるのでは
> なく、**両方が索引に入り、同じ文書について食い違う読み方が 2 つ並ぶ**。
> 「PDF に包んでから入れる」といった前処理は、体裁の問題ではなく索引の
> 中身を決める選択になる。

## 中身の要点

| | infographic | invoice | slide |
|---|---|---|---|
| block 数 | 48 | 6 | 13 |
| `chart` | **2** | 0 | 1 |
| `table` | 0 | **1** | **2** |
| `image` | 多数 (アイコン) | 1 | 8 |
| `markdown.text` の生 HTML | `div` / `img` | `div` / `img` / **`table` `tr` `td`** | `div` / `img` / **`table` `tr` `td`** |
| GFM のパイプ表記 | 無し | 無し | 無し |
| 末尾 LF | **1** | **0** | **1** |

**表は必ず HTML で来る。** GFM のパイプ記法は 1 本も現れない。**この 3 本が、
表を拒否するのをやめる根拠になった** — 07 §5 は元から表を GFM 記法と定めており、
足りていなかったのは「PaddleOCR-VL が何を送るか」の実測だけだったからである。
2026-08-06 から `convert_html_tables_to_gfm` がこの形を変換する (計画書 S3-L)。

> **形はここで確定している** — `border=1` / `<tr>` / `<td style='…'>` の 3 要素だけで、
> `rowspan` も `colspan` も `<th>` も入れ子も、3 本 11 行 38 セルに 1 つも無い。
> 変換するのはこの形だけで、外れたものは今までどおり拒否される。
>
> **見出し行は空にする。**invoice と slide 右の先頭行は見出しだが、**slide 左の
> 先頭行は本文**である (アイコン + `High text density` + 説明文)。`<th>` が無いので
> 応答からは区別できず、先頭行を昇格させると 3 本に 1 本で本文が列名として
> 07 §9 に凍る。これも「1 ページで成立した規則が次で成立しない」型である。

> ⚠ **採取直後は、そこへ到達すらしていなかった。**この 3 本は当時、生 HTML の
> 検査より前に `pair_images_with_boxes` の件数チェックで拒否されていた
> (19 参照/20 画像、1 参照/2 ボックス、11 参照/13 画像)。**表を 1 つも含まない
> infographic すら拒否されていた**ので、「表だから拒否される」という上の説明は
> 当時の挙動としては誤りだった。原因はペアリングの前提が実データで成り立たない
> ことで、`markdown.images` は figure ブロックと 1:1 の集合ではなく、
> **ネストした切り出しも含む全クロップの平坦な袋**である。
>
> `44842bd` の後に、各画像の**ファイル名が持つ bbox**
> (`img_in_<label>_box_<x0>_<y0>_<x1>_<y1>`) を読む方式へ直した。いまは 3 本とも
> パーサを通る。この訂正自体が、キャプチャを置いた理由の実例である。
> (その後 2026-08-06 に表の変換が入り、**3 本とも索引まで通る**ようになった。
> 拒否されていた期間に測れたのは「拒否の理由が 2 度とも思っていたのと違った」
> ことで、それがキャプチャの仕事だった。)

**末尾 LF は 0 / 1 の両方が出る** (別の機会には 2 も観測している)。
07 §5.2.1 はちょうど 1 個を要求するので、`normalize_to_markdown_v1` を
最後に掛ける処理 (`d66d063`) はここでも必要になる。

> ⚠ **`slide-single-figure.json` は名前どおりではない。** 由来の
> `slide-dense.png` には `table` が **2 つ**と `chart` が 1 つ入っており、
> 「図が 1 つだけのページ」ではない。単一図のケースが要るなら別の画像で
> 採り直すこと。名前は依頼された通りにしてある。

---

# 第 5 回の 5 本 (2026-08-09)

## これらは閾値の材料ではない

採りに行った目的は `related_images_min_area_ratio` の既定 **0.25** の根拠を増やすこと
だった。**1 件も増えていない。**理由は「閾値から外れていた」ではなく、
**そもそも閾値まで到達しない**ことである。

`images_with_their_own_boxes` が回すのは `markdown.images` ではなく
**`markdown.text` が参照した画像**である。出荷中のパーサに通した実測:

| キャプチャ | `markdown.images` | `<img>` 参照 | **Kio が見る画像** |
|---|---:|---:|---:|
| `code-editor-no-crops.json` | 0 | 0 | **0** |
| `terminal-dark-no-crops.json` | 0 | 0 | **0** |
| `whiteboard-no-crops.json` | 0 | 0 | **0** |
| `seal-crop-uncited.json` | **1** | **0** | **0** |
| `chat-light-avatar-crops.json` | 6 | 4 | 4 |
| (対照) infographic / invoice / slide / as-pdf | 20 / 1 / 13 / 21 | 19 / 1 / 11 / 20 | 19 / 1 / 11 / 20 |

`the_captures_separate_figures_from_decoration_around_the_shipped_floor` は
`boxes.iter().map(area).max().expect("a page with crops")` で分母を取るので、
**最初の 4 本はこのテストに入れると panic する。**図版が無いページは、
いまのテストの形にそもそも収まらない。

**収まらないこと自体が発見である。**閾値の下限は現在の設計では表現できていない。
図版が無いページでは**フィルタが 1 度も動かない**ので、「下限側で誤って何かを
図版と呼ぶ」余地がそもそも無い — ホワイトボードのマーカーもイレーサーも
反射も、1 つもクロップされなかった。

**`seal-crop-uncited.json` はその境目の実例である。**赤い「回覧」の押印は
クロップされる (`header_image`、箱 `805,174,921,285`) が、`markdown.text` が
1 度も参照しない。したがって Kio からは**テキストとしても画像としても**見えない。
ブリーフが「閾値の境目」と呼んでいた箱は、閾値まで届いていない。

> **置いただけでは検査対象にならない。**この 5 本を置いた状態で
> `cargo test -p kio-adapter --test real_layout_parsing_captures` は **7 passed**、
> そして**新しい 5 本は 1 バイトも読まれていない** (`include_str!` の const・
> `captures` 配列・`FIGURES` の 3 箇所が手書きのため)。この README の冒頭が
> 書いている「mock とコードが同意して CI が緑のまま」と同じ形が、
> ここでも成立している。テストの形は切り分けの結果で決める。

## キャプチャを足すときは `capture-manifest.json` に 1 行足す

**置くだけでは読まれない**問題は、`token_recall.rs` が塞いだ [2026-08-09]。
このディレクトリの `capture-manifest.json` が
「キャプチャ → 元画像 → 宣言トークンが戻るか」を機械可読で持ち、テストは
`include_str!` を使わず**実行時にディレクトリを走査**する。

- **行を足し忘れると落ちる。**`.json` がディレクトリに在って manifest に無ければ
  `every_capture_in_this_directory_is_read_by_this_test` が名前を挙げて失敗する
- **`token_in_markdown` は測定値であって目標ではない。**`false` は欠陥の記録である。
  サービスが改善して `true` になったら、テストは「良くなった。表を更新せよ」と
  落ちる。両方向の変化検出器で、どちらかを黙認する形にはしていない
- **`visible_text` の回収率は表示するが assert しない。**infographic が落とす 18 断片は
  すべてチャート内部の文字で、落ちるのが正常だからである

`real_layout_parsing_captures.rs` の方は今も手書き 3 箇所 (`include_str!` の const・
`captures`・`FIGURES`) のままである。あちらは**図が 1 つ以上あるページ専用**なので、
第 5 回の 4 本は入らない。入れると分母が無く panic する — その旨は panic
メッセージに書いてある。

## トークン照合 — `ground-truth.json` の宣言トークンは戻るか

**この検査は今まで 1 度もやっていなかった。**`experiments/ocr-verification/fixtures/images/ground-truth.json`
は各画像に埋め込まれたトークンを宣言している。照合は NFKC + 空白除去。

### 出荷済みの 4 本 (遡って検査)

| キャプチャ | 宣言トークン | `markdown.text` | 応答のどこか | `visible_text` 回収 |
|---|---|:---:|:---:|---:|
| `infographic-two-charts.json` | `G5-01-TOKEN-6843` | ✅ | ✅ | 36/54 |
| `invoice-table.json` | `G4-03-TOKEN-1950` | ✅ | ✅ | **9/9** |
| `slide-single-figure.json` | `G2-02-TOKEN-2905` | ✅ | ✅ | 45/47 |
| `infographic-two-charts-as-pdf.json` | `G5-01-TOKEN-6843` | ✅ | ✅ | 36/54 |

**出荷済みの 4 本はトークンを 1 つも落としていない。**infographic が落とす 18 断片は
すべて**チャートの内側の文字** (軸ラベル `1.00`…`0.00`、値 `0.78` `0.85` `0.90`、
系列名 `Run 1`…`Run 5`、`10%` `22%` `68%`) で、2 つの `chart` クロップへ入った分である。
これは欠陥ではなく、`related_images[]` と `kio open` が在る理由そのものである。

### 第 5 回の 5 本

| キャプチャ | 地色 | 宣言トークン | `markdown.text` | どこか | 回収 |
|---|---|---|:---:|:---:|---:|
| `code-editor-no-crops.json` | 暗 | `G1-01-TOKEN-4827` | ❌ | **❌** | 9/20 |
| `terminal-dark-no-crops.json` | 暗 | `G1-02-TOKEN-6194` | ✅ | ✅ | 6/7 |
| `chat-light-avatar-crops.json` | 明 | `G1-03-TOKEN-7358` | ✅ | ✅ | 8/10 |
| `whiteboard-no-crops.json` | 明 | `G3-02-TOKEN-9216` | ✅ | ✅ | 5/7 |
| `seal-crop-uncited.json` | 明 | `G4-02-TOKEN-7621` | ✅ | ✅ | 6/8 |

## `code-editor-no-crops.json` はトークンを失う — 原因は暗色地ではない

`code-editor.png` は `ground-truth.json` が **`expect: "text-dominant"`**
と分類する、最も易しいはずのページである。宣言トークン `G1-01-TOKEN-4827` は
**613 KB の応答のどこにも無い** — `markdown.text` にも、どの `block_content` にもである。
落ちたのはエディタ本文まるごと (関数定義・トークンのコメント行・日本語コメント)。
入力画像を開くと、その文字列は**大きな等幅フォントで画面の 7 割**を占めている。
読めなかったのは細かい文字ではなく、そのページで最も読みやすい部分である。

**極性の対照実験をした。暗色地が原因ではない。**

| | ブロック数 | `block_label` の内訳 | トークン |
|---|---:|---|:---:|
| `terminal-dark-no-crops.json` (**暗**) | 7 | `header` 1 / `text` 6 | ✅ |
| `chat-light-avatar-crops.json` (明) | 16 | `header` 1 / `image` 4 / `paragraph_title` 2 / `text` 6 / `footer_image` 2 / `footer` 1 | ✅ |
| `code-editor-no-crops.json` (**暗**) | **4** | **`content` 1** / `footer` 2 / `number` 1 | **❌** |

暗色のターミナルは 7 ブロックへ正しく分割され、トークンも戻る。
**分かれ目はブロック分割で、地色ではない。**

失敗した 1 本だけが `content` というラベルの箱を持ち、その箱は
`[0, 5, 1665, 867]` — **1672×941 のページの 99.6% × 92%**、つまり窓のほぼ全体である。
`content` は**9 本のキャプチャのうちこの 1 本にしか現れないラベル**である。
レイアウト検出が窓全体を 1 つの領域に潰した結果、その中の本文が転記されなかった。

> **UI スクリーンショット全般の問題でもない。**ターミナルもチャットも通っている。
> いまのところ「窓全体が 1 つの `content` 箱に潰れたページ」でだけ起きている。

## `header` / `footer` / `number` は `markdown.text` に載らない

`parsing_res_list` には本文があるのに `markdown.text` に出てこない箱がある。
**Kio が索引するのは `markdown.text` だけ**なので、この差はそのまま欠落になる。

| キャプチャ | `block_label` | `block_bbox` | `block_content` | `markdown.text` |
|---|---|---|---|:---:|
| `seal-crop-uncited.json` | `header` | `[508, 209, 678, 294]` | `'回覽'` | ❌ |
| `seal-crop-uncited.json` | `number` | `[413, 1117, 636, 1187]` | `'期限 7/10'` | ❌ |
| `code-editor-no-crops.json` | `footer` | `[1013, 893, 1572, 925]` | `'Ln 1, Col 1 Spaces: 2 UTF-8 LF JavaScript'` | ❌ |
| `code-editor-no-crops.json` | `footer` | `[33, 889, 238, 925]` | `'✗ 0 △0 (🔓) 0'` | ❌ |
| `code-editor-no-crops.json` | `number` | `[1598, 891, 1634, 924]` | `'{ }'` | ❌ |

**認識は成功している。**`block_content` は空ではなく、中身も合っている
(`回覽` は旧字体で来るが、位置も内容も正しい)。落ちているのは組み立ての側である。

`header` / `footer` / `number` を落とすこと自体は、ページ番号やヘッダを本文へ
混ぜないための妥当な規則である。**問題は本文が furniture として分類されたこと**で、
回覧文書の「期限 7/10」が `number` になるのは、規則ではなく分類の側の誤りである。

**ここは Kio 側で取り返せる。**中身は応答の中に在り、`markdown.text` を読むという
選択だけが捨てている。ただし**どう直すかはまだ決めない** — 本文と furniture を
どこで分けるかは、これ 1 例で決めてよい規則ではない。
