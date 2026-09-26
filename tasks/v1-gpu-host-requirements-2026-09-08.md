# v1実ローカルLLM試験用の実機準備

2026-09-08。利用候補は常時稼働する **RTX 4060 8GB搭載PC**。
利用者からの確認値は、WSL2 Ubuntu 26.04、既存Docker Desktop、SSD空き約556GiB、
VRAM総量8,188MiB・空き約7,265MiB、NVIDIAドライバー610.88。
物理RAM約32GiB、WSL割当約15GiB + swap 4GiB。このMacの登録済み`kio-lab`接続を
使う。クラウドGPUの調達は前提にしない。
本書は準備条件であり、実機への導入・runner登録・実モデル受入の完了記録ではない。

## 接続と実行環境

| 項目 | 準備条件 |
|---|---|
| OS | Linux x86_64が第一候補。Windowsの場合はWSL2 + UbuntuによるGPUコンテナ実行を検討する。既存OSの変更は前提にしない |
| 設定用SSH | このMacから到達できるホスト名/IP、ポート、ユーザー名。鍵認証を使い、サーバーのhost keyを確認する。LAN内の接続でよい |
| 作業用アカウント | 専用ユーザーと専用ディレクトリ。ドライバー等の初期導入権限と通常試験の権限を分ける |
| GPU | ホストおよびコンテナから `nvidia-smi` で認識されること。現在のPaddleOCRコンテナ経路はCUDA 12.6以降を扱えるドライバーが前提 |
| コンテナ | DockerとGPUコンテナ実行環境。LinuxはNVIDIA Container Toolkit、WindowsはWSL2 GPU連携を確認する |
| RAM・ディスク | 初期計画の目安としてRAM 32GB、SSD空き100GBを推奨。最低動作要件を実測した数値ではない。実際の容量と固定imageのサイズを確認して配分する |
| 常時運転 | 試験中はスリープしない。モデル取得とActions連携に必要な外向き通信が可能 |

今回のホストでは既存のWSL2とDocker Desktop連携を利用する。Ubuntu側へ別のDocker
daemonを追加する前提にせず、専用WSL環境から既存DockerのGPU実行能力を確認する。
2026-09-08に登録済み鍵とhost key照合を用いて接続を確認した。試験ユーザー、Ubuntu 26.04
x86_64、RTX 4060/8188MiB、ドライバー610.88、WSL約15GiB + swap 4GiB、Docker engine
29.6.2 / Compose 5.3.1を読み取り確認した。接続時の空きVRAMは7254MiB、C:空きは約558GiB。
WSL filesystemの表示上の空き容量は仮想diskの値であり、C:の物理空きを作業予算に使う。
接続先のIP、踏み台、鍵設定は利用者の端末側で管理し、repositoryへ複製しない。
この接続確認はモデル起動・推論の成功記録ではない。

Docker DesktopではWSL2 backendと対象UbuntuのWSL integrationを使用する。
GPUドライバーはWindows側を使用し、WSLへLinux用NVIDIAドライバーを追加しない。
専用WSLディストリビューションや専用ユーザーを用意しても、Docker Desktopのdaemonと
GPUは既存環境と共有される。Docker操作権限をOS全体からの隔離とみなさない。

導入前にUbuntu内で次を読み取り確認する。これらはimageのpullやcontainerの起動を行わない。

```sh
cat /etc/os-release
uname -r
nvidia-smi --query-gpu=name,memory.total,memory.free,driver_version --format=csv,noheader
free -h
df -h /
docker context show
docker version --format 'client={{.Client.Version}} server={{.Server.Version}} server_os={{.Server.Os}} server_arch={{.Server.Arch}}'
docker info --format 'os={{.OperatingSystem}} kernel={{.KernelVersion}} cpus={{.NCPU}} memory={{.MemTotal}} runtimes={{json .Runtimes}}'
docker compose version
docker system df
```

`free`が示すのはWSLに割り当てられたメモリであり、Windowsの物理RAM容量とは分けて記録する。
この確認だけではcontainer内のCUDA実行は証明できない。既存imageの確認後、固定したimageで
GPU実行を試験する。設定用SSHの到達性とhost keyは、その接続先が決まってから確認する。

秘密鍵・パスワード・APIキーを会話へ送る必要はない。接続設定時に利用する公開鍵を提示し、
利用者側へ登録する。既存のSSH設定を変更・置換せず、試験専用の接続設定を使う。

## Actionsからの経路

Macからの設定用SSHと、GitHub-hostedのWindows/Linux/macOSからの試験経路は別に準備する。
SSHやモデルAPIをインターネットへ直接公開する必要はない。

第一候補は、実機をモデルサーバーとして使い、受入workflowからVPN等の非公開経路と
転送先を制限したSSH tunnelで接続する構成。Kio側は認証したHTTPS peerへ接続し、
CA/trust登録とscopeの送信承認を通す。実機のGPUで計算したことと、各OSのKio clientが
動作したことを別々に記録し、モデルをmacOS/Windows上で実行したとは扱わない。

self-hosted runnerを使う代案ではGitHubへの外向きHTTPS・443番が必要になる。
公開PRの任意コードを普段使いの実機で実行させないよう、登録先・実行可能workflow・
アカウント・作業領域の隔離を具体化する。実機へのrunner導入はまだ行っていない。

## 8GBでの確認順序

既存の固定PaddleOCR-vLLM設定は12GB機で調整されたもので、8GBでの成功記録はない。
モデル重みのサイズだけでは、KV cache、画像処理、layoutモデルなどを含むピークVRAMを
判定できない。
さらにOCR構成自体が、pipeline APIとVLM serverの二つのGPU利用processを同時に起動する。
OCRと埋め込みの直列化に加え、この二つの合計使用量を確認する。既存の12GB向け設定を
上書きせず、8GBで測定した設定を別に保存する。Qwen側はmodel revisionと重みhashだけでなく、
server image/dependency一式の固定も必要である。

1. OS・ドライバー・GPUの空きVRAM・RAM・ディスクを読み取り確認する。
2. 固定モデルとruntimeでOCRだけを起動し、小さな公開PDF/画像を処理してピーク使用量を測る。
3. OCRを停止してVRAM解放を確認し、埋め込みモデルだけを同様に試験する。
4. 成功した設定を固定し、3 OSのKio clientから認証付き接続を確認する。
5. 8GBで成立しない場合は未受入として記録し、設定調整か既存12GB機の利用を検討する。

最新のupstreamにはCPU/Apple Silicon向け経路もある。ただし現在の固定vLLM構成とは
異なるruntimeであり、model一式・profile・HTTP envelope・使用資源を改めて確認する。
古いPaddleX 3.3の対応表だけを根拠に、現在のCPU/Arm対応を一律に否定しない。

## 一次資料

- [PaddleOCR-VLの環境・推論・サービス構成](https://github.com/PaddlePaddle/PaddleOCR/blob/main/docs/version3.x/pipeline_usage/PaddleOCR-VL.en.md)
- [NVIDIA Container Toolkit](https://docs.nvidia.com/datacenter/cloud-native/container-toolkit/latest/install-guide.html)
- [Docker DesktopのWSL2 GPU対応](https://docs.docker.com/desktop/features/gpu/)
- [CUDA on WSL](https://docs.nvidia.com/cuda/wsl-user-guide/index.html)
- [GitHub self-hosted runnerの通信要件](https://docs.github.com/en/actions/reference/runners/self-hosted-runners)
- [Kioで保存している固定構成](artifacts/paddleocr-vl-compose/compose.pinned.yaml)

## 2026-09-08 r33 実測チェックポイント

この追記は準備条件を受入完了へ変更しない。M0--M6の実装は広く進んでいるが、explicit
grant/trust、retained-FD traversal、ledger backup/restore の修正と再試験は継続中である。
r32 の full-workspace は fixture 関連を含む16 targetで失敗し、r33 の再試験は進行中で green
ではない。native local の A01/A02/A04 は candidate-zero の暫定診断が通っただけで、最終候補
や matrix 受入ではない。実Officeの10 ms監視問題は修正中であり、authenticated local のA08
executor、private path、Actions経路も未完了である。51-case matrix と verifier は存在するが、
同一candidateで両方が通った記録はない。

固定OCR実験は、public PDFに対する2回の request が HTTP 200 / `errorCode=0`、1 page、1 block、
0 image を返した限定的な実測である。`logId`を除くcanonical hash は
`b765ca3a8fd7967d1720e2acb2c97cc2ebdb5a1d8c4e3cf959aafcf33c54e608`。required weight hash は一致し、
4096 completion + 14 prompt token を収めるため `max_model_len=4160` を使った。peak VRAM は
6716 MiB、baseline/post-teardown はともに575 MiB、OOMは観測されなかった。証跡は
[summary](artifacts/v1-gpu-4060/ocr/measurement-summary.json)、
[measurement notes](artifacts/v1-gpu-4060/ocr/MEASUREMENT.md)、
[weight inventory](artifacts/v1-gpu-4060/ocr/layout-weights.inventory) にある。

Embedding固定実験は進行中であり、合格を主張しない。準備済みの固定inputは
[embedding artifact](artifacts/v1-gpu-4060/embedding/README.md)にある。このroundではpaid API spend、
Actions実行、pushを行っていない。manual acceptanceおよびPersonaScope/personaCorpus performanceも未主張である。

## 2026-09-08 r36/r37 checkpoint

The fixed embedding experiment reached the actual local HTTP/TLS service with
two identical text requests and two identical public-PNG image requests. Each
response was a finite 2,048-dimensional vector. The image pair was bitwise
identical, while native text output was not (`0.999998480837` cosine) and its
MRL-768 projection was also not (`0.999998786911` cosine). Peak VRAM was
7,213 MiB; teardown returned to 575 MiB used and 7,382 MiB free, with no OOM.
The evidence is
[`artifacts/v1-gpu-4060/embedding/results/attempt1-20260908T0948Z/RESULT.md`](artifacts/v1-gpu-4060/embedding/results/attempt1-20260908T0948Z/RESULT.md).
This is fixed-experiment resource and endpoint evidence, not an acceptance or
bitwise-determinism verdict.

Frozen-release macOS A07 LibreOffice passed DOCX, PPTX, XLSX, malformed-input
refusal, and no-outside-writes checks. Its local receipt is
`/private/tmp/kio-r36-local-acceptance/a07-office-receipt.json`. The observed
host LibreOffice was 26.8.0.3 while Actions is 26.2.5; the candidate SHA is zero
and evaluator-bound, so this remains local diagnostic evidence. Core A01 passed;
A02 native convergence failed and is under diagnosis, and A03 remains running.
The acceptance matrix is not green.

For r36, app-trust and OCR unit checks, `eval` (21), promotion (7), p3a (35),
self-heal (4), local OCR (12), and current-policy (9) checks passed. r37 step 3
reported 284 passes. The authenticated-local executor is wired but compilation
and correctness remain pending. Phased GPU TLS scripts are under implementation,
and the three-OS Actions route, runner, and secrets are not configured.

The requested review pattern is a Daybreak Blue security review followed by the
root agent's independent crosscheck. Blue identified a harness-config and
binary-fixture TOCTOU issue; its fix is in progress. No paid API calls, pushes,
or Actions runs occurred in this checkpoint.
