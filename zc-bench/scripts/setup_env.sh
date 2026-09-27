#!/usr/bin/env bash
# 측정 신뢰성 조건 고정. 사용: sudo ./scripts/setup_env.sh [iface] [mtu]
set -euo pipefail
IFACE="${1:-eth0}"
MTU="${2:-1500}"
# 플랜 §5.1: MTU 축은 1500 / 9000 두 번 실행한다.
#   sudo ./scripts/setup_env.sh eth0 1500
#   sudo ./scripts/setup_env.sh eth0 9000   (양쪽 모두 설정 후 ping -M do 로 확인)

echo "== CPU governor → performance =="
for c in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
  [ -w "$c" ] && echo performance > "$c" || true
done
grep -H . /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor 2>/dev/null | head -4 || true

echo "== NIC 기능 확인 =="
ethtool -i "$IFACE" | grep -E 'driver|version' || true
ethtool -k "$IFACE" | grep -E 'scatter-gather|tcp-segmentation|generic-segmentation' || true
echo "  ※ scatter-gather: off 이면 MSG_ZEROCOPY가 조용히 복사로 폴백됩니다"
ethtool -K "$IFACE" sg on 2>/dev/null || echo "  (sg on 설정 실패 — 드라이버 미지원일 수 있음)"

echo "== 하드웨어 타임스탬프 =="
ethtool -T "$IFACE" 2>/dev/null | head -12 || true

echo "== MTU → $MTU =="
ip link set "$IFACE" mtu "$MTU"
ip -br link show "$IFACE"

echo "== flow steering 지원 여부 (수신 zero-copy 가능성 확인) =="
ethtool -n "$IFACE" 2>&1 | head -3 || true

echo "== IRQ 분리 (NIC은 core0, 앱은 taskset -c 2,3 으로 실행) =="
for irq in $(grep -i "$IFACE" /proc/interrupts | awk -F: '{print $1}' | tr -d ' '); do
  echo 0 > "/proc/irq/$irq/smp_affinity_list" 2>/dev/null \
    && echo "  irq $irq → cpu0" || true
done

echo "== 열/전원 =="
command -v vcgencmd >/dev/null && {
  echo "  throttled: $(vcgencmd get_throttled)   (0x0 이어야 정상)"
  echo "  temp     : $(vcgencmd measure_temp)"
} || echo "  vcgencmd 없음 (RPi 아님)"

echo "done."
