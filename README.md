# Kio

> **Local-first knowledge archive, powered by frontier AI.**
> データはローカル、計算は最強の AI を使う。

Kio indexes the files you already have — PDFs, Office documents, images, code,
notes — and makes them searchable with evidence you can follow back to the
original bytes. Your originals, history, and index stay on your machine. The
heavy lifting (OCR, embedding) uses frontier models, on explicit opt-in.

Secondary framing: *evidence-grounded local knowledge archive*.

**Status: pre-release.** As of October 3, 2026, the latest published pre-release is
[`v0.1.0-rc.3`](https://github.com/ttokunaga-ja/kio/releases/tag/v0.1.0-rc.3).
Current development includes watch, managed restore, and recovery changes beyond
that release. v1 acceptance is incomplete; implementation, local checks,
three-OS acceptance, and packaged-binary verification are recorded separately in
[the implementation record](tasks/v1-implementation-progress.md).
The CLI surface can still change. There is no GUI.

---

## What it does differently

**1. Evidence Pointer.** Results point at `commit / tree / raw_hash /
chunk_hash / span`, not at a path. Moving, renaming, or normally deleting a file
does not change its citation identity. Resolving the citation still requires its
retained version and current scope policy; shallow GC, purge, or erase can make
it unavailable. See [citation retention](docs/08-evidence-pointer-spec.md#24-識別子の安定性と引用の保持).

**2. Markdown normalization.** Every format is converted to normalized Markdown,
so a human and a model read the same view of a scanned PDF, a slide deck, and a
source file.

**3. Content-addressed store.** Files are kept as CAS objects. Past versions and
normally deleted files can be accessed while the required history is retained
and current policy permits it. Tags protect their target versions from retention
shallowing; explicit `purge` / `erase` and missing history have separate effects.

## Who it is for

Developers, researchers, and engineers with a large, messy pile of local files,
who are comfortable with a CLI, and who want AI-assisted search without handing
the whole archive to someone else's cloud.

## Quick start

```bash
kio init
kio index --preview      # inspect what would be sent, before approving anything
kio index --offline --yes  # local ingest + baseline index; no HTTP dispatch
kio search "あの PDF"
kio open <pointer from the results>
```

The [`v0.1.0-rc.3` GitHub pre-release](https://github.com/ttokunaga-ja/kio/releases/tag/v0.1.0-rc.3)
is public. Its archive verification, installation, uninstallation, platform
support, and signing-status procedures are defined in
[docs/10-operations.md §12](docs/10-operations.md#12-rc-draft-artifact-の検証と導入).

## About network use

Kio is **local-first**, which means your originals, history, and index live on
your machine — it does not mean the tool refuses to use the network.

**Network transmission is off by default.** Nothing leaves your machine until
you explicitly opt in for a scope.

OCR and embedding send file contents to external adapters (Mistral OCR, Gemini,
and others). Run `kio index --preview` to inspect the network transmission
policy before you approve anything. Review `kio adapter approve <tool_id> --preview`,
then use `kio adapter approve <tool_id> --yes` to create the paired scope/device
grant. `index --yes` confirms local ingestion and does not create adapter grants;
existing grants may still allow HTTP calls unless `--offline` is specified.
`--online` does not create a grant. Kio logs which input was sent (identified by
hash), when, and to which adapter; it does not log the file contents or the API
request and response bodies.

Once you opt in, the contents of the affected files are transmitted to that
adapter, and **the provider's own terms govern what it does with them** —
including any retention or logging on their side. Local HTTPS adapters also
require an exact paired grant and managed local trust; `--offline` blocks their
HTTP calls as well as cloud calls. In-process deterministic adapters do not use
HTTP and can run with `--offline`.

See [docs/07-adapter-spec.md](docs/07-adapter-spec.md) for the opt-in scope,
lifetime, and revocation rules.

## Documentation

[docs/README.md](docs/README.md) is the entry point and defines the reading
order. The numbered files distinguish current implementation contracts, product
requirements, approved design directions, and planned additions. Implementation
follows the applicable contracts; a plan alone is not evidence of delivery.

The [approved knowledge UX plan](tasks/knowledge-ux-implementation-plan-2026-10-03.md)
keeps the CAS and immutable citations, completes existing v1 acceptance first,
then improves version-specific readiness, citation retention using existing tags,
and restore status. These additions are planned, not delivered functionality.

## Contributing

**Kio is not accepting external code contributions yet** — bug reports,
questions, and design feedback as issues are very welcome. See
[CONTRIBUTING.md](CONTRIBUTING.md) for the reasoning and for the terms that will
apply once contributions open.

## Security

Please report vulnerabilities privately — see [SECURITY.md](SECURITY.md). Do not
open a public issue for a security problem.

## License

Kio is source-available under the
[PolyForm Shield License 1.0.0](LICENSE.md).

> This section is a plain-language orientation, not legal advice. LICENSE.md is
> the controlling text; where this summary differs from it, LICENSE.md governs.

You may use, modify, and distribute Kio for **any purpose except providing a
product that competes with Kio, or with a product the licensor or its affiliates
provide using Kio**. Note that the license counts goods and services as
competing even when they are free of charge and even when they expose their
functionality through a different kind of interface.

Uses that are ordinarily fine — personal use, internal company use at any scale,
research, private forks, and building non-competing products on top of it — are
fine because they are not part of providing a competing product, not because the
category itself is exempt.

For competing use, a separate commercial license is available. See
[COMMERCIAL-LICENSE.md](COMMERCIAL-LICENSE.md).

The name "Kio" is a trademark and is not licensed by the source license — see
[TRADEMARKS.md](TRADEMARKS.md). **If you distribute a modified version, rename
it.**

Required Notice: Copyright 2026 TOKUNAGA Takumi (https://github.com/ttokunaga-ja/kio)

---

# 日本語

> 日本語は**参考訳**です。英語版との間に不一致がある場合は**英語版が優先**します。

Kio は、手元にある PDF・Office 文書・画像・コード・メモをそのまま索引化し、
**原文の根拠まで辿れる検索**を提供するローカル知識アーカイブです。データの主権は
あなたのマシンにあり、OCR や Embedding といった重い計算には frontier AI を使います
(明示的な opt-in 後)。

**状態: プレリリース。** 2026年10月3日時点の最新配布版は
[`v0.1.0-rc.3`](https://github.com/ttokunaga-ja/kio/releases/tag/v0.1.0-rc.3) です。
開発版には、その配布版以降の監視・管理対象復元・回復の変更があります。v1受入は未完了であり、
実装・ローカル検証・3 OS受入・配布物確認は [進捗記録](tasks/v1-implementation-progress.md) で
区別しています。CLI の仕様は変わりえます。GUI はありません。

## 中核 3 点

1. **Evidence Pointer** — 検索結果は path ではなく `commit / tree / raw_hash /
   chunk_hash / span` を指します。移動・改名・通常削除で引用の識別子は変わりません。
   解決には必要な版の保持と現行policyによる許可が必要です。
2. **Markdown 正規化** — 全形式を正規化 Markdown に変換し、人間と AI が同じビューを見ます。
3. **Content-addressed CAS** — 必要な履歴を保持し現行policyが許可する範囲で、過去版や通常削除した
   ファイルへ到達できます。tagは対象版の保持期限によるshallow化を防ぎます。明示的な
   `purge` / `erase`、履歴欠損の影響は別に扱います。

引用の保持範囲は [Evidence Pointer仕様](docs/08-evidence-pointer-spec.md#24-識別子の安定性と引用の保持)、
承認済みの改善工程は [保存・復元・引用保持の実装計画](tasks/knowledge-ux-implementation-plan-2026-10-03.md)
を参照してください。追加改善は未実装です。

## 対象ユーザー

大量の PDF・Markdown・コード・画像・研究資料を扱う開発者・研究者・技術者で、CLI に抵抗がなく、
AI 検索は試したいがアーカイブ全体をクラウドに丸投げしたくない層。

## ネットワーク利用について

**local-first** は「データの主権がローカルにある」という意味であり、「ネットを使わない」
という意味ではありません。

**ネットワーク送信は既定で無効です。** スコープごとに明示的に opt-in するまで、
データがマシンの外に出ることはありません。

OCR と Embedding では外部 Adapter へファイル内容が送信されます。承認の前に
`kio index --preview` で network transmission policy を確認してください。
`kio adapter approve <tool_id> --preview` を確認し、
`kio adapter approve <tool_id> --yes` で scope/device の paired grant を成立させます。
`index --yes` はローカル取り込みの確認を省略するもので、Adapter grant は作成しません。
既存の grant があれば HTTP 呼出は許可され得るため、ローカル処理に限る場合は
`kio index --offline --yes` を使います。`--online` も grant を作成しません。Kio が記録するのは
「どの入力を (hash で識別)・いつ・どの Adapter へ送ったか」であり、**ファイル本文や API の
リクエスト/レスポンス本体は記録しません**。

opt-in 後は対象ファイルの内容が当該 Adapter へ送信され、**送信先での取り扱い (保持・ログ等) は
その提供者の規約に従います**。local HTTPS Adapter にも exact paired grant と managed local trust が
必要で、`--offline` は cloud と local の両方の HTTP 呼出を禁止します。in-process の deterministic
Adapter は HTTP を使わず、`--offline` でも実行できます。

## 貢献

**現在、外部からのコード貢献は受け付けていません。** バグ報告・質問・設計へのフィードバックは
issue で歓迎します。理由と、受付開始後に適用予定の条件は [CONTRIBUTING.md](CONTRIBUTING.md) を
参照してください。

## セキュリティ

脆弱性は public issue ではなく、[SECURITY.md](SECURITY.md) の手順で**非公開でご報告ください**。

## ライセンス

Kio は [PolyForm Shield License 1.0.0](LICENSE.md) の下で source-available として公開しています。

> 本節は平易な説明であり、法的助言ではありません。効力を持つのは LICENSE.md であり、
> 本 README と異なる場合は **LICENSE.md が優先**します。

**Kio、または licensor もしくはその関連組織が Kio を用いて提供する製品と競合する製品の提供**を
除く、あらゆる用途で利用・改変・再頒布が可能です。ライセンス原文では、**無償で提供される場合も、
インターフェースの種類が異なる場合も競合に含まれる**点にご注意ください。

個人利用・企業規模を問わない社内利用・研究・非公開フォーク・非競合製品の開発が通常問題ないのは、
それらが競合製品の提供の一部ではないからであって、**その形態自体が一律に除外されるわけではありません**。
競合用途には別途 [商用ライセンス](COMMERCIAL-LICENSE.md) をご用意しています。

「Kio」の名称は商標であり、ソースライセンスでは許諾されません
([TRADEMARKS.md](TRADEMARKS.md))。**改変版を頒布する場合は改名してください。**
