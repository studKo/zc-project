# Linux/Rust 기반 Zero-Copy Raw 비디오 전송 성능 비교

RPi5에서 카메라로 캡처한 raw 영상을 네트워크로 전송할 때, **커널의 user↔kernel 메모리 복사를 제거하면 CPU 비용과 지연이 얼마나 개선되는지**를 Rust로 구현하고 실측하는 프로젝트입니다.

상세 설계와 근거는 [`docs/plan-v1.md`](docs/plan-v1.md)를 참고하십시오. 이 문서는 빠른 시작을 위한 개요와 설치 절차만 다룹니다.

---

## 1. 확정 환경 사양

| 항목 | 송신(Sender) | 수신(Receiver) |
|---|---|---|
| 장비 | RPi5 | NUC7i5BNH |
| NIC | RP1 내장 이더넷 (Cadence macb, GbE) | Intel I219-V (GbE) |
| OS | Ubuntu 24.04 LTS (arm64) | Ubuntu 24.04 LTS (amd64) |
| 연결 | 랜케이블 직결, 192.168.50.1 | 192.168.50.2 |

카메라(CSI)는 아직 연결되지 않아, 그 전까지는 합성 프레임으로 대체합니다. 영상 조건은 1280×960 NV12(1.84MB/frame)이며, 기본 30fps(442Mbps)입니다. fps 비교가 필요하면 24/30/60 중에서 고릅니다(24=354, 60=885 Mbps). 60fps는 GbE TCP 실효 대역폭(MTU 1500에서 약 941 Mbps)에 근접하고, 90fps(1,327 Mbps)는 링크 용량을 넘어 쓰지 않습니다.

---

## 2. Zero-copy 4개 층위

| 층위 | 의미 | 대표 기술 | 이 연구에서의 위치 |
|---|---|---|---|
| L0. 파이프라인 버퍼 | GStreamer 엘리먼트 간 복사 제거 | `v4l2src io-mode=dmabuf` | 동일 프로세스 내부, 검증 필요 |
| L1. 언어/메모리 | 바이트 슬라이스 무복사 파싱 | Rust `zerocopy` crate | 보조 실험 |
| L2. 프로세스 간 (동일 호스트) | 공유메모리 IPC | iceoryx2 | 이론적 상한 참조 |
| **L3. 호스트 간 (커널·NIC)** | user↔kernel 복사 제거 | `MSG_ZEROCOPY`, `io_uring SEND_ZC` | **주 실험 대상** |

**수신 측 zero-copy는 하드웨어 제약(header/data split, flow steering 미지원)으로 불가능하여, 송신(RPi5) 측만 측정 대상으로 합니다.** 근거는 `docs/plan-v1.md` §3.1을 참고하십시오.

GStreamer의 기본 네트워크 sink는 `MSG_ZEROCOPY`를 지원하지 않아, `appsink`으로 파이프라인에서 버퍼를 꺼내 Rust 전송 모듈로 넘기는 브릿지 구조를 씁니다.

---

## 3. Case 매트릭스 (요약)

| Case | 송신 경로 | 목적 |
|---|---|---|
| G0 | 기존 GStreamer sink | Baseline |
| G1 | `appsink` → 일반 `send()` | 파이프라인 우회 효과 분리 |
| G2 | `appsink` → `MSG_ZEROCOPY` | **zero-copy 효과 — 주 실험** |
| G3 | `appsink` → io_uring `SEND_ZC` | zero-copy + syscall 감소 |
| G4 | `v4l2src io-mode` 비교 | L0 동작 검증 |
| E | iceoryx2 | 이론적 상한 참조 |

상세 조건 축(MTU/fps/청크 크기)과 반복 조건은 `docs/plan-v1.md` §5를 참고하십시오.

---

## 4. 측정 지표 (요약)

**층 1 — 성능 지표**

| 순위 | 지표 |
|---|---|
| 1 | %CPU per Gbps |
| 2 | 커널 복사 시간 — 샘플링 추정 (프레임당 `copy_us_frame`, 보조: 점유율 `copy_time_pct`, 송신 함수 시간 `send_us_frame`) |

**층 2 — 설명 지표**

| 순위 | 지표 |
|---|---|
| 3 | 송신 함수 호출 수 (`send_calls`, 복사 함수 호출 수의 대체) |
| 4 | syscall 수 / 컨텍스트 스위치 |
| 5 | zero-copy 실제 성공률 (**무효 판정 기준**) |

> 지표 2·3은 RPi5 커널(6.8.0-1051-raspi)에서 복사 함수(`_copy_from_iter`, `__arch_copy_from_user` 등)에 kprobe를 걸 수 없어, 함수를 직접 프로브하는 대신 997 Hz 샘플링으로 "`zc-bench`가 `__arch_copy_from_user` 안에 있던 표본 비율"을 추정합니다. 표본 수가 적으면 오차가 크며(`bpf_samples` 참고), 복사 함수 호출 수는 잴 수 없어 `tcp_sendmsg_locked` 호출 수로 대체했습니다.
>
> summary.csv의 `lat_*` 컬럼은 `--echo` 사용 시 측정되는 **echo 왕복 지연의 1/2**로, 지표 2(커널 복사 시간)와는 다른 값입니다. 매트릭스(`run_matrix.sh`)는 `--echo`를 쓰지 않으므로 이 컬럼은 0입니다.

---

## 5. 저장소 구조

```
zc-project/
├── README.md                      ← 이 문서
├── docs/
│   └── plan-v1.md                 ← 전체 설계·근거 (통합본)
└── zc-bench/                      ← Rust 구현체 (Cargo 프로젝트)
    ├── Cargo.toml
    ├── src/
    │   ├── main.rs                 CLI, 송수신 루프, 결과 출력
    │   ├── frame.rs                프레임 헤더 정의 (zerocopy derive)
    │   ├── sink.rs                 NullSink / CopySink(G1) / MsgZcSink(G2)
    │   ├── zcstat.rs                errqueue 파서, zero-copy 성공률 검출 ★핵심
    │   ├── stats.rs                지연 히스토그램, CPU 집계, CSV 출력
    │   ├── util.rs                  공통 유틸
    │   └── gst_bridge.rs            (미작성) GStreamer appsink ↔ 전송 모듈 연결 — 설계 단계
    ├── scripts/
    │   ├── setup_env.sh            governor / IRQ affinity / MTU 설정
    │   ├── run_matrix.sh           실험 매트릭스 자동 실행 (cpu / copy 2패스)
    │   └── measure_copy.bt         지표 2·3 측정용 bpftrace 스크립트
    ├── analysis/
    │   ├── merge_copy.py           복사 점유율(bpftrace)·시스템 CPU(mpstat)를 summary.csv에 병합
    │   └── plot.py                 결과 그래프 생성
    └── data/                       측정 결과 (실행 시 생성)
```

> 현재 구현 상태: `frame.rs`, `sink.rs`, `zcstat.rs`, `stats.rs`, `util.rs`는 구현 완료. 2026-09-27 측정 도구 점검(청크 기록, 패스 분리, 워밍업 CPU 제외, bpftrace 프로브 교체 등)을 반영했으며, RPi5에서 §7.2~§7.4 순서로 재검증이 필요합니다. `gst_bridge.rs`는 아직 파일이 없고(설계 단계) 다음 작업 대상입니다. G3(io_uring `SEND_ZC`), G4, E(iceoryx2)도 미구현입니다.

---

## 6. 설치 방법

### 6.1 시스템 패키지 (RPi5, NUC 양쪽 공통)

```bash
sudo apt update
sudo apt install -y \
  build-essential pkg-config git curl \
  ethtool iproute2 net-tools tcpdump iperf3 \
  sysstat linuxptp bpftrace jq \
  python3-pip python3-venv

# 분석·그래프용 (결과를 그 자리에서 바로 분석하려면 필요, apt 버전으로 충분)
sudo apt install -y python3-pandas python3-matplotlib python3-numpy

# perf (라즈베리파이는 패키지명이 다를 수 있음)
sudo apt install -y "linux-tools-$(uname -r)" 2>/dev/null || \
sudo apt install -y linux-tools-raspi

# sysstat 데몬 활성화
sudo sed -i 's/ENABLED="false"/ENABLED="true"/' /etc/default/sysstat
sudo systemctl enable --now sysstat

# (RPi5만) vcgencmd — 스로틀·온도 판정에 필요. Ubuntu에는 기본 포함되지 않음
sudo apt install -y libraspberrypi-bin
sudo usermod -aG video "$USER"   # 일반 사용자로 vcgencmd 실행 (다시 로그인해야 적용)
vcgencmd get_throttled           # 일반 사용자로 throttled=0x... 가 나와야 함
```

### 6.2 GStreamer (RPi5만 필요)

```bash
sudo apt install -y \
  gstreamer1.0-tools gstreamer1.0-plugins-base gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-libav \
  libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev
```

### 6.3 Rust 툴체인 (양쪽)

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustc --version
```

### 6.4 저장소 클론 및 빌드

```bash
git clone <저장소 URL> zc-project
cd zc-project/zc-bench
cargo build --release
mkdir -p data
chmod +x scripts/*.sh
```

첫 빌드 후 생성되는 `zc-bench/Cargo.lock`은 저장소에 커밋합니다. 의존성 버전을 고정해야 다른 시점·장비에서도 같은 바이너리로 재현할 수 있습니다.

`Cargo.toml`의 주요 의존성:

```toml
[dependencies]
libc     = "0.2"
zerocopy = { version = "0.8", features = ["derive"] }
# gst_bridge.rs 구현 시 RPi5 쪽에 추가
# gstreamer     = "0.23"
# gstreamer-app = "0.23"
```

### 6.5 설치 확인

```bash
# 전송 없이 프레임 생성만 확인
./target/release/zc-bench --sink null --duration 10
```

---

## 7. 실행 순서

### 7.1 Phase 0 — 환경 확인

`docs/plan-v1.md` §8의 체크리스트를 RPi5와 NUC 양쪽에서 실행합니다. 특히 `ethtool -k eth0 | grep scatter-gather`가 `off`이면 이후 모든 측정이 무효화되므로 최우선 확인합니다.

### 7.2 zero-copy 검출기 검증 (루프백)

```bash
# RPi5에서 자기 자신에게 전송
./target/release/zc-bench --mode receiver --port 5001 &
./target/release/zc-bench --mode sender --sink msg_zc \
  --target 127.0.0.1:5001 --duration 30 \
  --case G2_loopback --out data/verify.csv
```

**`zero-copy : 0.00% [FALLBACK]`이 나오는 것이 정상입니다.** 커널이 루프백 수신 경로에서 강제로 복사하기 때문입니다. 이 결과가 나오면 검출기가 올바르게 동작한다는 뜻입니다.

> 검증 실행(§7.2·§7.3)은 `data/verify.csv`에 따로 기록합니다. 매트릭스 결과 파일(`data/summary.csv`)에 섞이면 `plot.py` 그래프에 `mtu 0` 패널과 검증용 케이스가 끼어듭니다.

### 7.3 RPi5 ↔ NUC 실제 네트워크

```bash
# NUC
./target/release/zc-bench --mode receiver --port 5001

# RPi5
./target/release/zc-bench --mode sender --sink msg_zc \
  --target 192.168.50.2:5001 --duration 60 \
  --case G2_real --out data/verify.csv
```

**여기서는 `100.00%`가 나와야 합니다.** 그 시점부터 Case 간 CPU·지연 차이가 유효한 실측값이 됩니다.
루프백 0%는 검출기가 맞는지만 확인할 뿐이고, G2 실험이 성립하는지는 이 실NIC 100%로만 확인됩니다. **이 단계를 통과하기 전에는 §7.4 매트릭스를 실행하지 않습니다.**

### 7.4 매트릭스 실행 (2패스)

지표 1(%CPU per Gbps)과 지표 2(커널 복사 시간)는 **분리해서 측정**합니다.
bpftrace 프로브 오버헤드가 CPU 수치를 부풀리기 때문입니다.

copy 패스는 복사 함수에 kprobe를 걸지 않고 샘플링을 쓰므로 심볼 확인은 필요 없습니다. 다만 `tcp_sendmsg_locked`에 kprobe가 붙는지는 확인합니다 (근거는 `scripts/measure_copy.bt` 머리말 참고).

```bash
sudo bpftrace -l 'kprobe:tcp_sendmsg_locked'   # 한 줄이 나와야 함
```

```bash
# MTU 1500 조건 (양쪽 모두 설정)
sudo ./scripts/setup_env.sh eth0 1500

# 패스 1 — 지표 1·4 (bpftrace 미사용)
./scripts/run_matrix.sh 192.168.50.2:5001 10 100 cpu

# 패스 2 — 지표 2·3 (bpftrace 동시 실행)
# 스크립트는 sudo 없이 실행합니다. 시작할 때 sudo 비밀번호를 한 번 묻고 bpftrace만 sudo로 띄웁니다.
./scripts/run_matrix.sh 192.168.50.2:5001 3 60 copy

# 병합 — 복사 점유율·송신 호출(copy 패스)과 시스템 전체 CPU(mpstat, 모든 패스)를 붙인다 → summary_full.csv
# cpu 패스만 돌렸어도 실행합니다 (지표 1b 계산에 필요).
python3 analysis/merge_copy.py data/summary.csv data/raw

# MTU 9000 조건으로 전환 후 위 과정 반복 (양쪽 모두 설정)
sudo ./scripts/setup_env.sh eth0 9000
```

fps는 기본 30 한 조건만 실행합니다. fps 비교가 필요하면 환경변수 `FPS`로 24/30/60 중에서 지정합니다. 그 밖의 값은 링크 용량을 넘는 조건이 섞이지 않도록 에러로 중단합니다.

```bash
FPS="24 30 60" ./scripts/run_matrix.sh 192.168.50.2:5001 10 100 cpu
```

`run_matrix.sh`는 시작할 때 다음을 확인하고, 하나라도 실패하면 중단합니다.

- 일반 사용자로 실행했는지 (sudo로 돌리면 `data/`가 root 소유가 되어 이후 실행이 실패함)
- `ping -M do -s <MTU-28>`으로 상대(NUC)까지 해당 MTU가 실제로 통과하는지 (MTU 불일치 시 점보 프레임이 조용히 버려짐)
- 기존 `data/summary.csv`가 현재 스키마인지 (이전 버전 파일이면 `mv data/summary.csv data/summary.csv.old` 후 재실행)

실행 순서는 조건마다 G1/G2를 번갈아(짝수 회차는 G2→G1) 돌려, RPi5 온도 상승이 한 케이스에 몰리지 않게 합니다. 각 실행의 로그(`*.log`), bpftrace 출력(`*.copy`, `*.copy.err`), 시스템 전체 CPU(`*.mpstat`, softirq 포함)는 `data/raw/`에 남습니다.

그래프 생성:

```bash
# §6.1에서 python3-pandas / python3-matplotlib / python3-numpy를 apt로 설치했다면 그대로 실행한다.
# (RPi5 Ubuntu 24.04의 apt 버전 pandas 2.1.4, matplotlib 3.6.3에서 동작 확인)
python3 analysis/plot.py data/summary_full.csv
```

`plot.py`는 zero-copy 성공률이 100%가 아니거나 기록이 없는 행, 실행 중 스로틀링이 발생한 행(`throttled`가 `0x0`/`n/a`가 아닌 행)을 자동으로 제외합니다. `n/a`는 `vcgencmd get_throttled`를 읽지 못한 실행(미설치, 또는 일반 사용자에게 `/dev/vcio` 권한 없음)으로 판정이 불가능해 제외하지 않으며, `plot.py`가 그 건수를 경고로 출력합니다. 스로틀 판정을 받으려면 RPi5에서 `vcgencmd get_throttled`가 일반 사용자로 동작하는지(보통 `video` 그룹 필요) 먼저 확인하십시오. 그래프는 MTU × fps 격자로 나뉘고, 막대는 반복 평균, 오차 막대는 95% 신뢰구간(t분포)입니다. 지표 1·4는 cpu 패스만, 지표 2·3은 copy 패스만 사용합니다.

추가 그래프와 경고:

- `0_achieved_fps.png` — 실제 달성 fps. 목표의 98% 미만인 조건은 경고로 출력합니다(링크 포화·버퍼 대기로 그 fps 조건이 성립하지 않음).
- `1b_sys_cpu_per_gbps.png`, `5b_zerocopy_gain_sys.png` — mpstat 기반 시스템 전체 CPU(softirq 포함) 기준 지표 1과 G1→G2 절감률. 프로세스 기준(`1`, `5`)과 크게 다르면 비용이 프로세스 밖(softirq 등)으로 옮겨갔다는 뜻입니다.
- msg_zc 실행에서 완료 통보가 전송 수보다 적으면(`zc_completed < zc_sent`) 경고합니다.

### 7.5 summary.csv 주요 컬럼

| 컬럼 | 의미 |
|---|---|
| `pass` | 측정 패스 (`cpu` / `copy`) |
| `mtu` | 송신 인터페이스 MTU (run_matrix가 기록, 수동 실행 시 `--mtu`) |
| `chunk_bytes` | 1회 전송 단위 (0 = 프레임 전체) |
| `frames` / `frames_total` | 워밍업 제외 / 포함 프레임 수 |
| `elapsed_s` | 측정 구간 (워밍업 종료 ~ 송신 루프 종료). `cpu_pct`도 이 구간 기준 |
| `run_ns` | zc-bench 총 실행 시간 (프로세스 시작 ~ flush 종료) — `copy_time_pct`의 분모 |
| `achieved_fps` | 측정 구간의 실제 전송 fps. 목표(`fps`)보다 낮으면 링크 포화·버퍼 대기 |
| `late_frames` | 측정 구간에서 한 주기(1/fps) 안에 전송을 끝내지 못한 프레임 수. 늦어지면 다음 프레임 기준 시각을 현재로 당겨 몰아 보내기(버스트)를 막음 |
| `zc_sent` / `zc_completed` | MSG_ZEROCOPY 전송 건수 / 완료 통보 수신 건수. 완료가 적으면 flush 제한 시간(2초) 안에 통보를 다 못 받은 것 |
| `throttled` | **이번 실행 중** 발생한 스로틀링 (현재 상태 bit0~3 + 실행 중 새로 켜진 이력 비트). `0x0`이어야 유효 |
| `temp_c` | 실행 종료 시점 SoC 온도 (°C) |

병합 후(`summary_full.csv`)에는 `bpf_samples`, `copy_samples`, `copy_cpu_pct`, `copy_us_frame`, `copy_time_pct`, `send_calls`, `send_us_frame`(bpftrace 샘플링·송신 호출)과 `sys_cpu_pct`, `sys_soft_pct`, `sys_cpu_per_gbps`(mpstat, 코어 1개 = 100% 단위로 `cpu_pct`와 같은 단위)가 추가됩니다.

헤더는 파일을 만들 때 한 번만 쓰이므로, zc-bench는 헤더가 현재 스키마와 다른 기존 파일에는 이어 쓰지 않고 에러로 중단합니다. 2026-09-29 스키마 변경(`achieved_fps`, `late_frames`, `zc_sent` 추가, `dropped` 제거) 이전에 만든 `data/summary.csv`는 `mv data/summary.csv data/summary.csv.old`로 옮긴 뒤 다시 측정하십시오.

---

## 8. 다음 작업

상세 항목은 `docs/plan-v1.md` §9를 참고하십시오. 요약:

1. `gst_bridge.rs` 구현
2. Case G0(기존 파이프라인) 재현으로 baseline 확보
3. G4로 L0(`dmabuf`) 실제 동작 검증
4. RPi5 ↔ NUC 1차 비교
5. NUC 하드웨어 타임스탬프로 정밀 지연 측정
6. 카메라 연결 시 합성 프레임 → 실측 전환
