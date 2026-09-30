# Zero-Copy Raw 비디오 전송 성능 비교 — 수행 계획서

> 이 문서는 기존 v1(2대+카메라 전제)·v2(단일 장비 대응)·v3(GStreamer 통합) 계획서를 하나로 통합한 최종 버전입니다. 개별 버전 문서는 폐기하고 이 문서로 대체합니다.

작성일: 2026-09-17
개정: 2026-09-27 — 측정 도구 점검 반영 (지표 2 정의, 2패스 분리, 실행 순서, 스로틀 판정, 신뢰구간)
개정: 2026-09-29 — fps 축을 링크 용량 안으로 조정(기본 30, 선택 24/30/60), 달성 fps·지연 프레임 기록, 시스템 전체 CPU(지표 1b) 추가

---

## 1. 목적

RPi5에서 카메라로 캡처한 raw 영상을 네트워크로 전송할 때, **커널의 user↔kernel 메모리 복사를 제거하면 CPU 비용과 지연이 얼마나 개선되는지**를 Rust로 구현하고 실측한다.

---

## 2. 확정 환경 사양

아래 사양은 **고정값**이며, 이후 실험은 전부 이 조합을 기준으로 한다.

| 항목 | 송신(Sender) | 수신(Receiver) |
|---|---|---|
| 장비 | RPi5 | NUC7i5BNH |
| NIC | RP1 내장 이더넷 (Cadence macb, GbE) | Intel I219-V (GbE) |
| OS | Ubuntu 24.04 LTS (arm64) | Ubuntu 24.04 LTS (amd64) |
| 역할 | 카메라 캡처 + zero-copy 전송 | 수신 + CPU/지연 측정 |
| 연결 | 랜케이블 직결 (스위치 없음), 192.168.50.1 | 192.168.50.2 |

**카메라(CSI)는 아직 연결되지 않은 상태다.** 연결 전까지는 합성 프레임(§6.1)으로 대체하고, 연결 시점에 V4L2 캡처 경로를 추가 검증한다.

NUC의 Intel I219-V는 하드웨어 타임스탬프를 지원하여, RPi5 단독으로는 불가능했던 **정밀한 편도 지연 측정**이 가능하다. 또한 GbE 대비 충분한 처리 여력이 있어 수신 측이 병목이 되지 않으므로, 송신(RPi5) 측 CPU 절감 효과를 깨끗하게 분리해서 볼 수 있다.

### 2.1 영상 조건 (고정)

```
해상도  : 1280 × 960
포맷    : NV12 (YUV420, 12bpp)
프레임  : 1,843,200 B ≈ 1.84 MB
fps     : 30 (기본). 비교 시 24 / 30 / 60 중에서 선택
대역폭  : 24fps = 354 Mbps, 30fps = 442 Mbps (1GbE 대비 약 44% — 링크 포화 없음), 60fps = 885 Mbps
```

GbE의 TCP 실효 대역폭은 약 941 Mbps(MTU 1500) / 990 Mbps(MTU 9000)다. 60fps는 MTU 1500에서 94%로 포화 직전이므로 `achieved_fps`로 목표 fps를 실제로 냈는지 확인한다. 90fps(1,327 Mbps)는 링크 용량을 넘어 "fps 비교"가 아니라 "링크 포화 테스트"가 되므로 쓰지 않는다(`run_matrix.sh`가 거부).

---

## 3. Zero-copy — 4개 층위

"Zero-copy"는 단일 기술이 아니라 서로 다른 층위를 가리키는 말이다. 이를 구분하지 않으면 결과 해석이 어긋난다.

| 층위 | 의미 | 대표 기술 | 이 연구에서의 위치 |
|---|---|---|---|
| **L0. 파이프라인 버퍼** | GStreamer 엘리먼트 간(카메라→인코더 등) 복사 제거 | `v4l2src io-mode=dmabuf` | 동일 프로세스 내부. 실제 동작 여부 검증 필요 |
| **L1. 언어/메모리** | 바이트 슬라이스를 복사 없이 타입으로 해석 | Rust `zerocopy` crate | 보조 실험 |
| **L2. 프로세스 간 (동일 호스트)** | 공유메모리 기반 IPC | iceoryx2 | 이론적 상한 참조용 |
| **L3. 호스트 간 (커널·NIC)** | user↔kernel 복사 제거, 네트워크 전송 | `MSG_ZEROCOPY`, `io_uring SEND_ZC` | **이 연구의 주 실험 대상** |

핵심 질문: **"L3를 적용하면 RPi5 → NUC 영상 전송에서 CPU와 지연이 얼마나 개선되는가."** L0/L1/L2는 비교·보조 실험이다.

### 3.1 수신 측 zero-copy는 불가능하다

수신 zero-copy(io_uring ZC Rx, `TCP_ZEROCOPY_RECEIVE`, AF_XDP)는 다음 하드웨어 조건을 요구한다.

1. Header/data split — 헤더와 페이로드를 분리해 각각 다른 곳에 기록
2. Flow steering — 연결별로 패킷을 다른 큐로 분류
3. 드라이버의 유저 페이지 매핑 지원

RPi5의 RP1 이더넷은 세 조건을 모두 지원하지 않는다. NUC의 Intel I219-V는 일반 사무용 NIC 계열로, 세 조건을 지원하지 않을 가능성이 높다 (Phase 0에서 `ethtool -n`으로 실측 확인, §8). Ubuntu 24.04 커널(6.8)도 io_uring ZC Rx 요구 버전(6.15+)에 못 미친다.

**결론: 이 연구는 송신(RPi5) 측 zero-copy만 측정 대상으로 한다.** 이는 실험 실패가 아니라 사전 조사로 확인한 유효한 기술적 제약이다.

---

## 4. GStreamer 파이프라인과의 연계

영상 캡처·전송은 GStreamer가 담당한다. GStreamer의 기본 네트워크 sink(`udpsink`, `tcpserversink`, `tcpclientsink`)는 GLib GSocket 기반의 표준 송신을 사용하며 **`MSG_ZEROCOPY`를 지원하지 않는다.** 따라서 sink만 교체해서는 zero-copy를 적용할 수 없다.

`appsink` 엘리먼트로 파이프라인에서 raw 프레임 버퍼를 꺼내, 별도 구현한 Rust 전송 모듈(`zc-bench`)로 넘기는 **브릿지 구조**를 쓴다. 수신 측은 받은 바이트를 필요 시 `appsrc`로 다시 파이프라인에 주입한다.

```
[RPi5]                                                    [NUC]
카메라(CSI, 미연결)
   │ io-mode=dmabuf (L0, 검증 필요)
   ▼
v4l2src → GStreamer 파이프라인 ──┬─[G0] 기존 sink ═══════▶ 기존 수신 파이프라인
                                  │   (udpsink/tcpserversink)
                                  │
                                  └─[G1~G3] appsink ──▶ zc-bench 송신 ═══▶ zc-bench 수신 ──▶ appsrc(필요시)
```

---

## 5. 실험 Case 매트릭스

| Case | 송신 경로 | 목적 |
|---|---|---|
| **G0** | 기존 GStreamer 기본 sink (`udpsink`/`tcpserversink`) | **현재 운영 방식. Baseline** |
| **G1** | `appsink` → 일반 `send()` (커스텀) | G0과 비교해 "파이프라인 우회" 자체의 효과 분리 |
| **G2** | `appsink` → `MSG_ZEROCOPY` (커스텀) | **L3 zero-copy 효과 — 주 실험** |
| **G3** | `appsink` → io_uring `SEND_ZC` (커스텀) | zero-copy + syscall 감소 복합 효과 |
| **G4** (보조) | `v4l2src io-mode=mmap` vs `dmabuf` (네트워크 미포함) | L0 자체가 실제로 동작하는지 검증 |
| **E** (보조) | iceoryx2 (동일 호스트) | 복사 0의 이론적 상한 참조 |

G0→G1→G2를 단계적으로 비교하는 이유: G0과 G2를 바로 비교하면 "파이프라인을 우회해서 빨라진 것"과 "zero-copy라서 빨라진 것"이 섞인다. G1을 사이에 둬서 두 효과를 분리한다.

```
G0 → G1 : 파이프라인 우회 효과
G1 → G2 : zero-copy 순수 효과  ← 증명하고자 하는 값
```

측정 도구상 Case G1/G2는 `--sink copy` / `--sink msg_zc`에 대응하며, `--chunk` 옵션으로 전송 단위를 제어한다. GStreamer 연동(`gst_bridge.rs`) 완성 전까지는 합성 프레임으로 동일 경로를 측정한다.

### 5.1 조건 축 (독립변수)

| 축 | 값 |
|---|---|
| MTU | 1500 / 9000 |
| fps | 기본 30. 비교 시 24 / 30 / 60 (`FPS="24 30 60" ./scripts/run_matrix.sh ...`) |
| 청크 크기 | 64KB / 460KB / 1.84MB |

각 조합 10회 반복 × 100초, 워밍업 5초 제외(데이터와 CPU 시간 모두). CPU governor는 양쪽 모두 `performance`로 고정한다.

반복은 조건마다 G1/G2를 **번갈아** 실행한다(홀수 회차 G1→G2, 짝수 회차 G2→G1). G1 10회를 다 돌고 G2 10회를 도는 순서면 RPi5 온도 상승(열 드리프트)이 G1/G2 차이에 섞이기 때문이다. 결과 그래프는 반복 평균과 95% 신뢰구간(t분포)을 함께 표시해, G1/G2 차이가 실행 간 편차보다 큰지 판단한다.

송신 루프는 한 주기(1/fps) 안에 전송을 끝내지 못한 프레임을 `late_frames`로 세고, 다음 프레임 기준 시각을 현재로 당겨 뒤처진 만큼 몰아 보내는 버스트를 막는다. 측정 구간의 실제 fps는 `achieved_fps`로 기록한다.

매트릭스 실행 전 `run_matrix.sh`가 `ping -M do -s <MTU-28> <상대 IP>`로 경로 MTU를 확인한다. 상대(NUC) MTU가 다르면 점보 프레임이 조용히 버려지는데도 결과에는 해당 MTU로 기록되기 때문이다.

### 5.2 2패스 측정

지표 1과 지표 2는 분리해서 측정한다. bpftrace가 997 Hz 샘플링과 `tcp_sendmsg_locked` 진입·반환 프로브를 동시에 돌리므로, 같이 돌리면 그 오버헤드가 지표 1(%CPU per Gbps)에 섞여 들어간다.

| 패스 | 대상 지표 | 실행 |
|---|---|---|
| cpu | 1, 4, 5 | `./scripts/run_matrix.sh <target> 10 100 cpu` |
| copy | 2, 3 | `./scripts/run_matrix.sh <target> 3 60 copy` (sudo 없이 실행 — 스크립트가 bpftrace만 sudo로 띄움) |

두 패스는 `tag`(패스 이름 포함)와 summary.csv의 `pass` 컬럼으로 구분된다. copy 패스 결과는 `analysis/merge_copy.py`로 `tag`를 키 삼아 copy 패스 행에만 결합하며, 그래프에서 지표 1·4는 cpu 패스만, 지표 2·3은 copy 패스만 사용한다.

---

## 6. 측정 지표

지표를 "결과(성능)"와 "원인(설명)" 두 층으로 나눈다. pps·MTU·fps 같은 조건값은 결과 지표로 취급하지 않는다.

**층 1 — 성능 지표 (무엇이 좋아졌는가)**

| 순위 | 지표 | 근거 |
|---|---|---|
| 1 | %CPU per Gbps | 조건이 달라져도 비교 가능한 유일한 정규화 지표 |
| 2 | 커널 복사 시간 (샘플링 추정) | 커널이 프레임 복사에 소요한 시간. 주 지표는 **프레임당 복사 시간** `copy_us_frame = copy_ns_est / frames_total`, 보조 지표는 **점유율** `copy_time_pct = copy_ns_est / run_ns`와 **송신 함수 시간** `send_us_frame`(`tcp_sendmsg_locked` 체류, 복사 외 처리 포함). zero-copy 성립을 가장 직접적으로 증명하는 근거값 |

**층 2 — 설명 지표 (왜 그런 결과가 나왔는가)**

| 순위 | 지표 | 비고 |
|---|---|---|
| 3 | 송신 함수 호출 수 (`tcp_sendmsg_locked`, `bpftrace`) | 복사 함수 호출 수는 잴 수 없어 대체한 지표. 2번(커널 복사 시간)과 동일 스크립트에서 동시 산출 |
| 4 | syscall 수 / 컨텍스트 스위치 | |
| 5 | zero-copy 실제 성공률 | **무효 판정 기준.** 100% 미만이면 해당 실행 폐기 |

5번을 강조하는 이유: `MSG_ZEROCOPY`는 전제조건(scatter-gather 등)이 안 맞으면 **에러 없이 조용히 일반 복사로 되돌아간다.** 이를 확인하지 않으면 "효과가 없다"는 결론이 사실은 "zero-copy가 동작하지 않았다"는 오류일 수 있다. Case G0/G1은 커널 zero-copy를 쓰지 않으므로 5번 지표 대상에서 제외한다.

**지표 2 측정 방법 (샘플링).** 처음 계획은 TCP 송신의 복사 함수(`_copy_from_iter`, `_copy_from_iter_nocache`)에 kprobe를 걸어 체류 시간을 재는 것이었으나, RPi5 커널(6.8.0-1051-raspi, arm64)에서는 이 함수들과 `__arch_copy_from_user`, `skb_do_copy_data_nocache`에 kprobe가 붙지 않는다(`cannot attach kprobe, Invalid argument`). `csum_and_copy_from_iter_full`은 `tx-checksumming: on`이라 송신 경로가 아니고, `do_copy_data_nocache`는 붙지만 호출되지 않는다. 그래서 함수를 직접 프로브하는 대신 bpftrace `profile:hz:997`로 `comm == "zc-bench"`가 CPU 위에 있는 순간의 커널 함수를 세고, 그중 `__arch_copy_from_user`에 있던 표본의 비율로 복사 시간을 추정한다(`copy_cpu_pct` = 프로세스 CPU 시간 중 복사 비율). 루프백 시험에서 G1은 표본의 약 23.5%가 이 함수에 있었고 G2는 0이었다. 통계적 추정이므로 표본 수(`bpf_samples`)가 200개 미만인 행은 경고한다. bpftrace는 워밍업을 포함한 실행 전체를 관측하므로, 분모도 워밍업 포함 전체 전송 프레임 수(`frames_total`)와 zc-bench 총 실행 시간(`run_ns`)을 쓴다. bpftrace 자체의 `wall_ns`는 부착 대기와 timeout 여유분이 섞여 실제 실행보다 길므로 분모로 쓰지 않는다.

**지표 3 (대체).** 복사 함수를 프로브할 수 없으므로 호출 수도 잴 수 없다. kprobe가 붙는 `tcp_sendmsg_locked`의 호출 수와 체류 시간을 대신 기록한다. 이 함수는 복사 외에 skb 할당 등 송신 처리 전체를 포함하므로 "복사 시간"이 아니라 "송신 함수 시간"이다.

**지표 1의 한계.** `%CPU`는 송신 프로세스의 CPU 시간(getrusage, user+sys)만 센다. `setup_env.sh`가 NIC IRQ를 core0에 고정하므로, 다른 코어의 softirq에서 처리되는 `MSG_ZEROCOPY` 완료 통보 비용 등은 지표 1에 잡히지 않을 수 있다(G2에 유리한 방향의 편향). 이를 교차 확인하도록 `run_matrix.sh`가 실행마다 `mpstat -P ALL 1`로 시스템 전체 CPU(%soft 포함)를 `data/raw/<tag>.mpstat`에 함께 기록하고, `merge_copy.py`가 측정 구간 평균을 `sys_cpu_pct`(코어 1개 = 100% 단위)로 붙인다. 이를 goodput으로 나눈 `sys_cpu_per_gbps`를 **지표 1b**로 보고, 프로세스 기준(지표 1)과 G1→G2 절감률을 비교한다. 두 절감률이 크게 다르면 비용이 프로세스 밖(softirq 등)으로 옮겨간 것이다. 시스템 전체 값이라 다른 프로세스의 부하도 섞이므로 cpu 패스에서만 쓴다.

**G2 구현 비용 최소화.** `MsgZcSink`는 완료 통보(errqueue)를 청크마다가 아니라 프레임당 1회 수거하고, 빈 버퍼를 기다릴 때는 `poll`로 잠든다(스핀 없음). 청크마다 `recvmsg`를 부르거나 스핀 대기하면 zero-copy와 무관한 시스템콜 비용이 G2의 지표 1에 더해져 효과를 과소평가하게 된다. 완료 통보가 flush 제한 시간 안에 다 오지 않은 실행은 `zc_completed < zc_sent`로 드러난다.

**echo 지연과의 구분.** summary.csv의 `lat_*` 컬럼은 `--echo` 사용 시 측정되는 echo 왕복 지연의 1/2로, 지표 2(커널 복사 시간)와는 다른 값이다.

### 6.1 합성 프레임 소스 (카메라 미연결 대응)

카메라가 연결되기 전까지, 정적 패턴에 프레임 헤더(`seq`, `tx_ns`)만 매 프레임 갱신하는 합성 프레임을 사용한다. 프레임 생성 비용은 `NullSink`로 별도 측정해 전체 CPU에서 분리한다. 카메라 연결 시 V4L2 캡처 경로(`io-mode=dmabuf`/`mmap`)로 교체하고 §5의 G4로 비교 검증한다.

---

## 7. 검증 우선 원칙

성능 수치를 산출하기 전에 다음 순서로 도구 자체를 먼저 검증한다.

```
① 환경 가용성 확인 (Phase 0)
       ↓
② zero-copy 검출기 정상 동작 확인 (루프백)
       ↓
③ RPi5 ↔ NUC 실제 네트워크로 zero-copy 성공률 확인
       ↓
④ 성능 지표 측정
       ↓
⑤ 설명 지표로 원인 규명
```

②에서 루프백은 `zc_success_pct = 0%`가 나오는 것이 정상이다(커널이 수신 경로에서 강제 복사). 이 예측이 실측과 일치하는지 먼저 확인해, 측정 도구 자체의 신뢰성을 데이터보다 먼저 확보한다. ③에서 실제 네트워크(RPi5→NUC)로 `100%`가 나와야 그 이후 수치가 유효해진다.

---

## 8. Phase 0 — 환경 확인 (RPi5 + NUC 양쪽)

```bash
# 커널 버전, io_uring 지원 (양쪽)
uname -r
grep CONFIG_IO_URING /boot/config-$(uname -r)

# zero-copy 전제조건 — off면 MSG_ZEROCOPY가 조용히 일반 복사로 폴백 (최우선 확인)
ethtool -k eth0 | grep -E 'scatter-gather|segmentation'

# 지표 2·3용 bpftrace 프로브 (RPi5) — 복사 함수는 kprobe 불가, 샘플링과 tcp_sendmsg_locked를 쓴다
sudo bpftrace -l 'kprobe:tcp_sendmsg_locked'   # 한 줄이 나와야 함
uname -r; ethtool -k eth0 | grep tx-checksumming

# 드라이버, flow steering, 하드웨어 타임스탬프
ethtool -i eth0 | grep driver
ethtool -n eth0                      # flow steering 지원 여부 — NUC에서도 확인
ethtool -T eth0                      # NUC는 HW 타임스탬프 지원 예상

# 점보 프레임 가능 여부 (직결 상태에서 양쪽 설정)
sudo ip link set eth0 mtu 9000
ping -M do -s 8972 <상대 IP>

# 링크 상한
iperf3 -s                            # NUC
iperf3 -c 192.168.50.2 -t 60 -P 1    # RPi5

# 열/전원 (RPi5만 해당) — 0x0이 아니면 CPU 수치 전체가 무효
# Ubuntu에는 vcgencmd가 없으므로 먼저 설치: sudo apt install libraspberrypi-bin
# 일반 사용자로 실행돼야 zc-bench가 실행마다 스로틀을 판정할 수 있다 (sudo usermod -aG video $USER)
vcgencmd get_throttled
vcgencmd measure_temp

# irqbalance — 켜져 있으면 setup_env.sh의 IRQ 고정을 덮어쓴다 (setup_env.sh가 중지)
systemctl is-active irqbalance

# GStreamer 및 필요 플러그인 확인 (RPi5)
gst-launch-1.0 --version
gst-inspect-1.0 appsink appsrc v4l2src udpsink tcpserversink | grep -A2 "Factory Details"
```

`scatter-gather: off`가 나오면 `sudo ethtool -K eth0 sg on`을 시도하고, 안 되면 실험 설계를 재검토한다.

---

## 9. 다음 작업

1. `gst_bridge.rs` 구현 — `appsink`에서 꺼낸 버퍼가 `MSG_ZEROCOPY` 요구 조건(페이지 정렬)을 만족하는지 확인 후 연결
2. Case G0(기존 GStreamer 파이프라인) 현재 장비에서 재현 — baseline 확보
3. Case G4로 L0(`v4l2src io-mode=dmabuf`) 실제 동작 여부 검증
4. RPi5 ↔ NUC 실네트워크에서 Case G1/G2 1차 비교, `zc_success_pct` 확인
5. NUC 하드웨어 타임스탬프 기반 정밀 편도 지연 측정 적용
6. 카메라(CSI) 연결 시 합성 프레임 → V4L2 실측으로 전환

---

## 10. 리스크

| 리스크 | 징후 | 대응 |
|---|---|---|
| RPi5 scatter-gather 미지원 | `zc_success_pct` 0% (실네트워크) | `ethtool -K eth0 sg on` 시도, 불가 시 Case G2/G3 재검토 |
| appsink 버퍼가 페이지 정렬 불충족 | G2에서 성공률 0%/폴백 | `CopySink`로 한 번 정렬 후 `MsgZcSink`에 전달하는 폴백 경로 사용 |
| GStreamer dmabuf 연쇄 시 내부 복사 발생 | G4에서 CPU 차이 없음 | "L0 미동작"으로 결론, 실패 아닌 유효한 조사 결과로 기록 |
| RPi5 서멀 스로틀링 | summary.csv `throttled` ≠ 0x0 (실행 중 발생분만 기록 — `get_throttled`의 부팅 후 누적 비트는 실행 전후 차이로만 반영) | 능동 쿨러 필수, 해당 실행은 `plot.py`가 자동 제외. `temp_c`로 온도 추이 확인 |
| 442Mbps 구간에서 zero-copy 단독 효과 미미 | Case 간 CPU 차이 수 % 이내 | MTU 9000 및 fps 60(885 Mbps)으로 부하를 높여 효과 크기 확인. 60fps는 포화 직전이므로 `achieved_fps` 확인 |

---

## 부록. 참고 자료

- `zerocopy` crate — https://google.github.io/zerocopy/zerocopy/index.html
- google/zerocopy — https://github.com/google/zerocopy
- iceoryx2 — https://github.com/eclipse-iceoryx/iceoryx2
- MSG_ZEROCOPY 커널 문서 — https://docs.kernel.org/networking/msg_zerocopy.html
- io_uring zero-copy Rx — https://docs.kernel.org/networking/iou-zcrx.html
- GStreamer udpsink — https://gstreamer.freedesktop.org/documentation/udp/udpsink.html
- v4l2src DMABUF 연쇄 시 복사 발생 이슈 — https://github.com/raspberrypi/firmware/issues/1769
- Zero-Copy Pages in Rust — https://redixhumayun.github.io/databases/2026/04/14/zero-copy-pages-in-rust.html
