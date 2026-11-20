# Contributing to Kio

> ## Kio is not accepting external code contributions yet
>
> Kio is pre-release and maintained by one person. The contribution process —
> in particular the Contributor License Agreement below — has not been through
> legal review, and it would be unfair to ask you to sign something we are not
> yet confident in. **Pull requests are therefore not being merged at this
> time.**
>
> **What is welcome right now:** bug reports, questions, and design feedback as
> issues. Security problems should go through [SECURITY.md](SECURITY.md), not a
> public issue.
>
> The rest of this document describes the process we intend to use once
> contributions open. It is published so you can see the terms in advance, not
> because they are in force. Watch the repository for the announcement.

Thank you for considering a contribution. Please read the
[Contributor License Agreement](#contributor-license-agreement) below before
opening a pull request — Kio is dual-licensed, and that has consequences for
what you are agreeing to.

---

## Before you write code

`docs/` is the **normative specification**, not documentation generated after
the fact. The numbered files are the source of truth for Kio's behavior, and the
implementation is expected to follow them. Read
[docs/README.md](docs/README.md) first; it defines the reading order.

This means:

- **A behavior change usually needs a spec change first.** Open an issue
  describing the behavior you think is wrong or missing before writing the
  patch. A PR that changes behavior without touching the spec will be read as a
  bug report against the spec.
- **A bug fix that restores spec-conformant behavior needs no spec change.**
  Cite the spec section it restores.
- Hash-bearing identities (`tool_profile_hash`, `prompt_template_hash`,
  `raw_hash`, chunk and embedding identity) are contracts. Changing an input to
  any of them invalidates stored artifacts. If your change moves a frozen hash
  vector, say so explicitly in the PR — do not simply update the expected value.

## Building and testing

```bash
cargo test --workspace --all-targets --locked
```

The full check that CI runs:

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --all-targets --locked
```

The canonical evaluator, cross-scope supplement, offline rerank scorer,
and scale fixture lifecycle are Rust (`kio-eval`) and
are covered by the workspace tests. Push and pull-request CI executes no
Python. The Rust exception-ledger test requires every tracked `.py` file to be
an explicit manual Python-native ML/SDK/render adapter:

```bash
cargo test -p kio-eval --all-targets --locked
```

High-cost Full/cold validation, Full scale acceptance, U7 GPU comparison, and
manual OCR fixture/provider commands have one explicit entry point:
[Manual full and cold Rust gates](tasks/manual-full-cold-gates.md). None is an
ignored-test substitute or an automatically scheduled Actions lane.

Development, CI, and the workspace contract all use Rust `1.98.0` with
Edition 2024 (see `rust-toolchain.toml` and the workspace `Cargo.toml`). Kio is
pre-stable and does not maintain a second MSRV compatibility contract.

## Pull requests

- Keep one concern per PR. A rename, a refactor, and a fix are three PRs.
- Match the surrounding code — its naming, its comment density, its idiom.
- Tests that would have caught the bug are part of the fix.
- If you could not verify something, say so in the PR. An honest "I did not test
  the Windows path" is far more useful than silence.

---

## Contributor License Agreement

By submitting a contribution to this project, you agree to the following terms
for that contribution and every contribution you have previously submitted.

> **The English text of this Contributor License Agreement is the binding text.**
> The Japanese section at the end of this file is a partial summary provided for
> convenience only; it is not a complete translation, and where the two differ,
> the English text controls. If you cannot read the English text, please do not
> submit a contribution.
>
> This section is a license grant, not legal advice. If you are contributing on
> behalf of an employer, or you are unsure whether you hold the rights described
> below, please get advice before submitting.

**1. Definitions.** "You" means the individual or legal entity submitting a
contribution. "Contribution" means any work of authorship you intentionally
submit to this project, in any form and through any channel, for inclusion in or
documentation of Kio. "Licensor" means TOKUNAGA Takumi.

**2. You keep your copyright.** This agreement transfers no ownership. You
remain free to use, license, and relicense your contribution however you wish,
including in other projects and under other terms.

**3. Copyright license.** You grant the licensor a perpetual, worldwide,
non-exclusive, irrevocable, royalty-free, transferable license, with the right
to sublicense through multiple tiers, to reproduce, prepare derivative works of,
publicly display, publicly perform, and distribute your contribution and such
derivative works, **under any license terms the licensor chooses, including the
[PolyForm Shield License 1.0.0](LICENSE.md), other open source licenses, and
proprietary or commercial terms**.

This last clause is the point of this agreement. Kio is offered both under a
source-available license and under negotiated commercial licenses
([COMMERCIAL-LICENSE.md](COMMERCIAL-LICENSE.md)). The licensor cannot offer a
commercial license covering your contribution without your permission to do so.

If you are not willing to grant that, **do not submit code, patches, tests,
documentation text, or other material for inclusion.** You may still open an
issue limited to a high-level description of the problem or the behavior you
want; such an issue is not a contribution under section 1 and grants no license
to its text, and the licensor may implement the underlying idea independently.

**4. Patent license.** You grant the licensor and every recipient of Kio a
perpetual, worldwide, non-exclusive, irrevocable, royalty-free patent license to
make, have made, use, offer to sell, sell, import, and otherwise transfer your
contribution, covering only those patent claims you can license that are
necessarily infringed by your contribution alone or by its combination with Kio.
If you institute patent litigation alleging that Kio or a contribution
constitutes patent infringement, the patent licenses you were granted for Kio
terminate as of the date the litigation is filed.

**5. Your representations.** You represent that:

- each contribution is your original creation, or you have the right to submit
  it under these terms;
- you are legally entitled to grant the licenses above, and if your employer has
  rights to intellectual property you create, you have received permission to
  make the contribution or your employer has waived those rights;
- your contribution does not, to your knowledge, violate anyone's rights; and
- if any part of your contribution is not your original creation, you have
  identified its source, license, and any restrictions in the submission.

**6. No obligation.** The licensor is under no obligation to accept, merge, or
retain any contribution.

**7. No warranty.** Except for the representations in section 5, you provide
your contribution "as is", without warranty of any kind, express or implied.

### How to signify agreement

Add a `Signed-off-by` line to each commit, which you can do automatically:

```bash
git commit -s
```

The sign-off certifies that you have read and agree to the Contributor License
Agreement above for that contribution.

---

# 日本語

> ## Kio は現在、外部からのコード貢献を受け付けていません
>
> Kio はプレリリースであり、単独メンテナで開発しています。以下の CLA は法務レビューを
> 受けておらず、確信の持てない条件への同意をお願いするのは不適切と考えるため、
> **現時点で Pull Request はマージしません**。
>
> **歓迎するもの**: バグ報告・質問・設計へのフィードバック (issue でどうぞ)。
> セキュリティの問題は public issue ではなく [SECURITY.md](SECURITY.md) の手順でお願いします。
>
> 以下は貢献を受け付け始めた際に適用する予定の手続きです。事前に条件を確認できるよう
> 公開しているもので、現在効力を持つものではありません。

> **重要**: 拘束力を持つのは**英文の CLA 全文**です。以下の日本語は便宜のための
> **部分的な要約であり、完全な対訳ではありません**。英文には、遡及適用・取消不能性・譲渡可能性・
> 多階層のサブライセンス・第三者素材の開示・無保証など、下記に記載のない条項が含まれます。
> 両者が異なる場合は**英文が優先**します。**英文を理解できない場合は、貢献を提出しないでください。**
> 本節は参考訳であり、法的助言ではありません。

貢献をご検討いただきありがとうございます。Kio は**デュアルライセンス**のため、
Pull Request を出す前に上記の **Contributor License Agreement (CLA) 英文全文**を必ずお読みください。

## 実装の前に

`docs/` は後付けの説明書ではなく **仕様の正本**です。まず
[docs/README.md](docs/README.md) を読み、定義された順序で参照してください。

- **挙動を変える変更は、原則として先に仕様変更が必要です。** まず issue を立ててください。
- **仕様どおりの挙動を回復するバグ修正**には仕様変更は不要です。根拠となる節を示してください。
- ハッシュ同一性 (`tool_profile_hash` / `prompt_template_hash` / `raw_hash` / chunk・embedding
  identity) は契約です。凍結されたハッシュ期待値が動く変更は、期待値を黙って書き換えず、
  **PR に明示してください**。保存済み artifact が無効化されます。

## ビルドと検証

```bash
cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --all-targets --locked
```

## CLA の要点

- **著作権はあなたに残ります。** 譲渡ではありません。
- licensor に対し、あなたの貢献を **商用ライセンスを含む任意の条件で再ライセンスできる権利**を
  許諾していただきます。これが本 CLA の核心です。Kio は
  [商用ライセンス](COMMERCIAL-LICENSE.md) も提供しており、この許諾がないと、あなたの貢献を
  含む形で商用ライセンスを提供できません。
- 特許ライセンスの許諾と、提出物があなたの原著作物であること (または提出する権利があること)、
  雇用主の権利がある場合は許可を得ていることの表明が含まれます。
- **この条件に同意できない場合は、PR を出さずに issue で変更内容をご提案ください。**

同意の表明は、各コミットへの `Signed-off-by` 行 (`git commit -s`) をもって行います。
