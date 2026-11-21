# Codex への作業指示

- **作業ディレクトリ** — <worktree の絶対パス>。変更はこの範囲内で行う。
- **読む資料** — `AGENTS.md`、該当する [v1 計画](../v1-plan.md)・[製品仕様](../../docs/README.md)・[決定記録](../../docs/decisions.md)、<対象に固有の資料>。
- **作業範囲と受入条件** — <変更する対象、対象外、期待する結果>。
- **出力先** — <成果物の相対パス>。一時物は worktree の `scratch/validate/<作業名>/`、ログは `scratch/logs/<作業名>/`、保存する報告は `scratch/reports/<作業名>/` に置く。作業で一時ディレクトリが必要なら、`TMPDIR` を検証ディレクトリ内に設定する。
- **Git 操作** — 統括が差分を確かめて統合する。委譲先の作業は編集と検証までとし、stage・commit・push・branch・worktree の操作は統括が行う。
- **実装** — ふるまいを変えるときは、期待動作のテストが意図した理由で失敗することを確かめてから実装する。現行の契約と回帰テストを守り、不要になった経路を片付ける。
- **検証** — <対象のコマンドと期待結果>。`cargo fmt --all -- --check`、`cargo clippy -p <crate> --all-targets --locked -- -D warnings`、`cargo test -p <crate> --all-targets --locked` を変更範囲に応じて実行する。検証には `cargo run -q -p kio-ci --bin kio-code-limits --locked -- --root .`（終了値 0）を含める。全 workspace のテストは統括がサンドボックス外で回す。文書・移動は関連パスとリンクを確かめる。
- **報告** — 日本語で変更と削除、テストの Red→Green または文書の検証、実行したコマンドと結果、未確認・保留と理由を記す。仕様と実装の食い違いも報告する。それぞれの判断と停止条件は `AGENTS.md` に従う。
