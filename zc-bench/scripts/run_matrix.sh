#!/usr/bin/env bash
# 실험 매트릭스 자동 실행 (RPi5 송신 → NUC 수신).
#
# 플랜 §5.1 조건 축을 그대로 따른다:
#   - 해상도 1280x960 NV12 고정 (프레임 1,843,200 B)
#   - 청크 크기 : 64KB / 460KB / 1.84MB
#   - fps       : 기본 30 한 조건. 비교 시 환경변수 FPS로 24/30/60 중에서 선택
#                 (24=354 / 30=442 / 60=885 Mbps — 60은 GbE MTU 1500 실효 대역폭 ~941 Mbps에 근접.
#                  그 이상(예: 90=1,327 Mbps)은 링크 용량을 넘어 허용하지 않는다)
#   - MTU       : 1500 / 9000  ← setup_env.sh로 양쪽 전환 후 각각 1회씩 실행
#
# 2패스 구조:
#   pass=cpu   지표 1(%CPU per Gbps), 4(컨텍스트 스위치) — bpftrace 미사용 (프로브 오버헤드 배제)
#   pass=copy  지표 2(프레임당 커널 복사 시간), 3(복사 호출 수) — bpftrace 동시 실행
#   tag와 summary.csv의 pass 컬럼으로 두 패스를 구분하므로 같은 파일에 있어도 섞이지 않는다.
#
# 실행 순서 (청크·fps 조합마다):
#   반복(run01..N) 루프가 바깥, 케이스(G1/G2) 루프가 안쪽 — G1/G2를 번갈아 돌려
#   RPi5 온도 상승(열 드리프트)이 한 케이스에만 몰리지 않게 한다.
#   짝수 회차는 G2→G1로 순서를 뒤집어(ABBA) "항상 먼저 도는 쪽" 편향도 상쇄한다.
#
# 권한:
#   일반 사용자로 실행한다. copy 패스는 시작할 때 sudo 인증을 한 번 캐시하고
#   bpftrace만 sudo로 띄운다 → data/ 아래 파일은 모두 사용자 소유로 남는다.
#
# 전제 조건 (README §7.3):
#   RPi5 → NUC 실네트워크에서 --sink msg_zc 1회 실행 시 zero-copy 100%가 확인되어야 한다.
#   통과 전에는 이 스크립트를 실행하지 않는다.
#
# 사용:
#   ./scripts/run_matrix.sh [수신IP:포트] [반복횟수] [측정초] [pass]
#   ./scripts/run_matrix.sh 192.168.50.2:5001 10 100 cpu
#   ./scripts/run_matrix.sh 192.168.50.2:5001 3 60 copy
#   FPS="24 30 60" ./scripts/run_matrix.sh 192.168.50.2:5001 10 100 cpu   # fps 비교
#   송신 인터페이스는 환경변수 IFACE로 지정 (기본 eth0)
set -euo pipefail

# 상대 경로(target/, scripts/, data/)는 zc-bench/ 기준
cd "$(dirname "$0")/.."

TARGET="${1:-192.168.50.2:5001}"
REPEAT="${2:-10}"
DURATION="${3:-100}"
PASS="${4:-cpu}"
WARMUP=5
IFACE="${IFACE:-eth0}"
BIN="./target/release/zc-bench"
OUT="data/summary.csv"
RAWDIR="data/raw"
HOST="${TARGET%:*}"
PORT="${TARGET##*:}"
SESSION="$(date +%m%d%H%M)"   # 같은 조건을 다시 돌려도 tag가 겹치지 않게

# 고정 사양 (플랜 §2.1)
WIDTH=1280
HEIGHT=960
FRAME_BYTES=$((WIDTH * HEIGHT * 3 / 2 + 40))   # NV12 페이로드 + 헤더 40 B

# 조건 축 — 순서가 고정되도록 인덱스 배열 사용
CHUNK_KEYS=(64K 460K full)
CHUNK_BYTES=(65536 471040 0)
read -r -a FPS_LIST <<< "${FPS:-30}"   # 기본 30fps 한 조건, FPS="24 30 60" 으로 비교
# Case G1 = appsink→일반 send 상당, G2 = appsink→MSG_ZEROCOPY 상당
CASE_KEYS=(G1 G2)
CASE_SINKS=(copy msg_zc)

case "$PASS" in
  cpu|copy) ;;
  *) echo "[중단] pass는 cpu 또는 copy 여야 합니다: $PASS"; exit 1 ;;
esac

for v in "$REPEAT" "$DURATION"; do
  [[ "$v" =~ ^[0-9]+$ ]] || { echo "[중단] 반복횟수·측정초는 정수여야 합니다: $v"; exit 1; }
done

# fps는 GbE 링크 용량 안의 값만 허용 — 넘는 조건은 "fps 비교"가 아니라 "링크 포화 테스트"가 된다
[ "${#FPS_LIST[@]}" -gt 0 ] || { echo "[중단] FPS가 비어 있습니다"; exit 1; }
for f in "${FPS_LIST[@]}"; do
  case "$f" in
    24|30|60) ;;
    *) echo "[중단] 허용하지 않는 fps: $f (24/30/60 중에서 선택, 기본 30)"; exit 1 ;;
  esac
done

# zc-bench의 --duration은 워밍업을 포함한다 → 워밍업 이하이면 측정 구간이 0
if [ "$DURATION" -le "$WARMUP" ]; then
  echo "[중단] 측정초($DURATION)는 워밍업(${WARMUP}s)보다 커야 합니다."
  exit 1
fi

if [ "$(id -u)" -eq 0 ]; then
  echo "[중단] 일반 사용자로 실행하세요. copy 패스는 bpftrace만 내부에서 sudo로 띄웁니다."
  echo "       (sudo로 돌리면 data/ 가 root 소유가 되어 이후 cpu 패스가 실패합니다)"
  exit 1
fi

[ -x "$BIN" ] || { echo "[중단] $BIN 이 없습니다 — cargo build --release 먼저 실행"; exit 1; }

# ── data/ 쓰기 권한 확인 (이전 sudo 실행 흔적) ─────────────────────
for p in data "$RAWDIR" "$OUT"; do
  if [ -e "$p" ] && [ ! -w "$p" ]; then
    echo "[중단] $p 에 쓰기 권한이 없습니다 (이전 sudo 실행 흔적)."
    echo "       sudo chown -R \"$(id -un)\" data  후 다시 실행하세요."
    exit 1
  fi
done
mkdir -p "$RAWDIR"

# ── summary.csv 스키마 확인 (이전 버전 파일에는 이어 쓰지 않는다) ──
if [ -s "$OUT" ]; then
  hdr=",$(head -n 1 "$OUT" | tr -d '\r'),"
  if [[ "$hdr" != *,pass,* || "$hdr" != *,achieved_fps,* || "$hdr" != *,zc_sent,* ]]; then
    echo "[중단] $OUT 은 이전 스키마(pass/achieved_fps/zc_sent 컬럼 없음) 파일입니다."
    echo "       mv $OUT $OUT.old  후 다시 실행하세요."
    exit 1
  fi
fi

# ── 경로 MTU 확인: 상대(NUC) MTU가 다르면 점보 프레임이 조용히 버려진다 ──
MTU="$(cat "/sys/class/net/$IFACE/mtu" 2>/dev/null)" \
  || { echo "[중단] 인터페이스 $IFACE 가 없습니다 (IFACE=<이름> 으로 지정)"; exit 1; }
PING_SIZE=$((MTU - 28))   # IPv4 헤더 20 + ICMP 헤더 8
echo "== 경로 MTU 확인: $IFACE mtu=$MTU → ping -M do -s $PING_SIZE $HOST =="
if ! ping -M do -c 3 -W 2 -s "$PING_SIZE" "$HOST" >/dev/null 2>&1; then
  echo "[중단] $HOST 까지 $MTU 바이트 패킷이 단편화 없이 통과하지 않습니다."
  echo "       상대(NUC) MTU가 다를 가능성이 큽니다. 양쪽에서 setup_env.sh <iface> $MTU 로 맞춘 뒤 다시 실행하세요."
  exit 1
fi
echo "  OK"

# ── 시스템 전체 CPU 기록 (softirq·IRQ 포함 — 프로세스 CPU만 보는 지표 1의 교차 확인용) ──
HAVE_MPSTAT=0
if command -v mpstat >/dev/null; then
  HAVE_MPSTAT=1
else
  echo "[참고] mpstat 없음 — 시스템 전체 CPU 기록을 생략합니다 (sudo apt install sysstat 권장)"
fi

# ── 백그라운드 프로세스 정리 (Ctrl-C 포함) ──────────────────────────
KEEPALIVE=""
MPID=""
cleanup() {
  if [ -n "$KEEPALIVE" ]; then kill "$KEEPALIVE" 2>/dev/null || true; fi
  if [ -n "$MPID" ]; then kill "$MPID" 2>/dev/null || true; fi
}
trap cleanup EXIT

if [ "$PASS" = "copy" ]; then
  command -v bpftrace >/dev/null || { echo "[중단] bpftrace 미설치"; exit 1; }
  echo "copy 패스: bpftrace 실행용 sudo 인증을 캐시합니다."
  sudo -v
  # 매트릭스는 수 시간 걸린다 — sudo 캐시(기본 15분)가 만료되면 백그라운드 sudo가
  # 비밀번호를 기다리며 멈추므로 주기적으로 갱신한다.
  while sleep 60; do sudo -n true || exit; done &
  KEEPALIVE=$!
fi

echo "target=$TARGET repeat=$REPEAT duration=${DURATION}s pass=$PASS mtu=$MTU session=$SESSION"
for f in "${FPS_LIST[@]}"; do
  echo "  fps $f → 예상 $((FRAME_BYTES * 8 * f / 1000000)) Mbps (GbE TCP 실효 ~941 Mbps @MTU1500)"
done
echo "※ 수신 측(NUC)에서 먼저 실행: $BIN --mode receiver --port $PORT"
echo

# 기저값: 프레임 생성 비용 (전송 없음) — CPU 지표용이므로 cpu 패스에서만.
# 매트릭스 실행과 같은 코어(2,3)에 고정해야 비교 가능하다.
if [ "$PASS" = "cpu" ]; then
  taskset -c 2,3 "$BIN" --mode sender --sink null --duration 20 --warmup 3 \
       --case N --tag "cpu_N_baseline_mtu${MTU}_s${SESSION}" --pass cpu --mtu "$MTU" \
       --fps 30 --width "$WIDTH" --height "$HEIGHT" \
       --out "$OUT" >/dev/null
  echo "[baseline] null sink 완료"
fi

TOTAL=0
FAILED=0

# 1회 실행: $1=case $2=sink $3=청크 라벨 $4=청크 바이트 $5=fps $6=회차
run_one() {
  local ckase="$1" sink="$2" ckey="$3" csize="$4" fps="$5" r="$6"
  local tag
  tag="${PASS}_${ckase}_mtu${MTU}_${ckey}_fps${fps}_run$(printf '%02d' "$r")_s${SESSION}"
  local log="$RAWDIR/${tag}.log"
  local btpid=""
  TOTAL=$((TOTAL + 1))
  echo "[$tag] ..."

  if [ "$PASS" = "copy" ]; then
    # timeout이 SIGINT로 bpftrace를 스스로 끝낸다 → END 블록(wall_ns, 합계) 출력.
    # 일반 사용자는 root 소유 프로세스에 kill -INT를 보낼 수 없으므로 kill 대신 timeout을 쓴다.
    # 리다이렉션은 이 셸(사용자)이 하므로 결과 파일은 사용자 소유다.
    # -B line: 파일로 리다이렉션해도 BEGIN 출력이 바로 보이게 (부착 완료 확인용)
    sudo -n timeout --signal=INT $((DURATION + WARMUP + 5)) \
      bpftrace -B line scripts/measure_copy.bt \
      > "$RAWDIR/${tag}.copy" 2> "$RAWDIR/${tag}.copy.err" &
    btpid=$!
    # 프로브 부착 대기 — BEGIN은 모든 프로브가 붙은 뒤 실행된다 (최대 10초)
    local waited=0
    until grep -q 'probe started' "$RAWDIR/${tag}.copy" 2>/dev/null; do
      if ! ps -p "$btpid" >/dev/null 2>&1; then
        echo "  [경고] bpftrace 시작 실패 — $RAWDIR/${tag}.copy.err 확인"
        break
      fi
      if [ "$waited" -ge 20 ]; then
        echo "  [경고] 10초 안에 bpftrace 부착을 확인하지 못했습니다 — 초반 복사가 누락될 수 있음"
        break
      fi
      sleep 0.5
      waited=$((waited + 1))
    done
  fi

  if [ "$HAVE_MPSTAT" = 1 ]; then
    LC_ALL=C mpstat -P ALL 1 > "$RAWDIR/${tag}.mpstat" 2>&1 &
    MPID=$!
  fi

  if taskset -c 2,3 "$BIN" --mode sender \
       --sink "$sink" --target "$TARGET" \
       --width "$WIDTH" --height "$HEIGHT" --fps "$fps" --chunk "$csize" \
       --duration "$DURATION" --warmup "$WARMUP" \
       --case "$ckase" --tag "$tag" --pass "$PASS" --mtu "$MTU" \
       --out "$OUT" --raw "$RAWDIR/${tag}.csv" > "$log" 2>&1; then
    grep -E 'CPU per Gbps|achieved fps|zero-copy|throttled|경고' "$log" || true
  else
    FAILED=$((FAILED + 1))
    echo "  [실패] 로그: $log"
    tail -n 5 "$log" | sed 's/^/    /'
  fi

  if [ -n "$MPID" ]; then
    kill "$MPID" 2>/dev/null || true
    wait "$MPID" 2>/dev/null || true
    MPID=""
  fi

  if [ -n "$btpid" ]; then
    # zc-bench가 끝나기 전에 timeout이 먼저 bpftrace를 끝냈다면 후반 복사가 빠진다
    if ! ps -p "$btpid" >/dev/null 2>&1; then
      echo "  [경고] bpftrace가 zc-bench보다 먼저 종료됨 — 복사 시간이 과소 집계됐을 수 있음"
    fi
    wait "$btpid" 2>/dev/null || true   # timeout 만료까지 (수 초) 대기
    if ! grep -q '^wall_ns' "$RAWDIR/${tag}.copy"; then
      echo "  [경고] bpftrace 결과 없음 — $RAWDIR/${tag}.copy.err 확인"
    fi
  fi

  sleep 3   # 열 안정화
}

for ci in "${!CHUNK_KEYS[@]}"; do
  for fps in "${FPS_LIST[@]}"; do
    for ((r = 1; r <= REPEAT; r++)); do
      if (( r % 2 == 1 )); then order=(0 1); else order=(1 0); fi
      for k in "${order[@]}"; do
        run_one "${CASE_KEYS[$k]}" "${CASE_SINKS[$k]}" \
                "${CHUNK_KEYS[$ci]}" "${CHUNK_BYTES[$ci]}" "$fps" "$r"
      done
    done
  done
done

echo
echo "완료: $((TOTAL - FAILED))/$TOTAL 성공. 요약: $OUT"
if [ "$PASS" = "copy" ]; then
  echo "복사 시간 병합: python3 analysis/merge_copy.py $OUT $RAWDIR"
fi
echo "그래프: python3 analysis/plot.py data/summary_full.csv   (copy 패스 전이면 $OUT)"
echo "  zero-copy 100% 미만·실행 중 스로틀 발생 행은 plot.py가 자동 제외합니다."
if [ "$FAILED" -gt 0 ]; then
  echo "[경고] 실패 $FAILED 건 — $RAWDIR/*.log 확인"
  exit 1
fi
