#!/usr/bin/env python3
"""zc-bench 결과 그래프 생성.

사용:
  python3 analysis/plot.py data/summary_full.csv [출력디렉터리]
  (복사 시간 병합 전이면 data/summary.csv 도 가능 — 지표 2·3 그래프는 생략됨)

플랜 §6 지표 체계에 대응하는 그래프
(모두 MTU × fps 격자, 막대 = 반복 평균, 오차 막대 = 95% 신뢰구간)
  0.  실제 달성 fps                  — 조건 성립 확인            [pass=cpu]
  1.  %CPU per Gbps                 — 층1 지표 1 (주 지표)     [pass=cpu]
  1b. 시스템 전체 %CPU per Gbps      — 지표 1 교차 확인(softirq 포함, mpstat 병합 후) [pass=cpu]
  2.  프레임당 커널 복사 시간 (주)   — 층1 지표 2               [pass=copy]
  2b. 커널 복사 시간 점유율 (보조)   — 층1 지표 2               [pass=copy]
  3.  복사 함수 호출 수              — 층2 지표 3               [pass=copy]
  4.  컨텍스트 스위치                — 층2 지표 4               [pass=cpu]
  5.  G1 → G2 효과 분리 요약         — zero-copy 순수 효과      [pass=cpu] (5b: 시스템 CPU 기준)

지표 1·4는 cpu 패스만, 지표 2·3은 copy 패스만 쓴다 (bpftrace 오버헤드가 지표 1에 섞이지 않도록).

제외 규칙
  - msg_zc 실행인데 zero-copy 성공률이 100% 미만이거나 기록이 없는(NaN) 행 — 지표 5 무효 판정
  - throttled 가 0x0 / n/a 가 아닌 행 — 실행 중 스로틀링 발생
  - null sink 행 — 프레임 생성 비용 기저값 (비교 대상 아님)

경고만 하는 항목 (제외하지 않음)
  - 달성 fps가 목표의 98% 미만인 조건 — 링크 포화·버퍼 대기로 "그 fps" 조건이 성립하지 않았음
  - msg_zc 실행 중 완료 통보가 전송 수보다 적은 행 (zc_completed < zc_sent)
"""
import math
import os
import sys

import matplotlib
import pandas as pd

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402

SUMMARY = sys.argv[1] if len(sys.argv) > 1 else "data/summary_full.csv"
OUTDIR = sys.argv[2] if len(sys.argv) > 2 else "data/plots"
os.makedirs(OUTDIR, exist_ok=True)

if not os.path.exists(SUMMARY):
    sys.exit(f"파일이 없습니다: {SUMMARY}")

# 양측 95% t 값. 키 = 표본 수 n (자유도 n-1). scipy 의존을 피하려고 표로 둔다.
# copy 패스는 3회(n=3), cpu 패스는 10회(n=10) 반복이 기본이다.
T95 = {
    2: 12.706, 3: 4.303, 4: 3.182, 5: 2.776, 6: 2.571, 7: 2.447, 8: 2.365, 9: 2.306,
    10: 2.262, 11: 2.228, 12: 2.201, 13: 2.179, 14: 2.160, 15: 2.145, 16: 2.131,
    17: 2.120, 18: 2.110, 19: 2.101, 20: 2.093, 21: 2.086, 22: 2.080, 23: 2.074,
    24: 2.069, 25: 2.064, 26: 2.060, 27: 2.056, 28: 2.052, 29: 2.048, 30: 2.045,
    31: 2.042,
}
_t_warned = set()


def t95(n):
    if n < 2:
        return float("nan")
    if n in T95:
        return T95[n]
    if n not in _t_warned:
        print(f"[참고] n={n}의 t값이 표에 없어 1.96(정규 근사)을 사용합니다.")
        _t_warned.add(n)
    return 1.96


# pandas 기본값은 "null"·"n/a"를 결측치로 읽어 sink="null" 필터와 throttled 판정이 깨진다.
# zc-bench가 쓰는 "NaN"과 빈 칸(병합 전 copy 컬럼)만 결측으로 본다.
df = pd.read_csv(SUMMARY, keep_default_na=False, na_values=["NaN", ""])

REQUIRED = {"pass", "mtu", "fps", "case", "sink", "chunk_bytes", "zc_success_pct",
            "throttled", "cpu_per_gbps", "ctx_vol", "ctx_invol", "tag",
            "achieved_fps", "zc_sent", "zc_completed"}
missing = REQUIRED - set(df.columns)
if missing:
    sys.exit(f"{SUMMARY}에 {sorted(missing)} 컬럼이 없습니다 — 이전 스키마 파일입니다. "
             "새 zc-bench로 다시 측정하세요.")

df = df[df["sink"] != "null"].copy()

# ── 무효 실행 제외 ───────────────────────────────────────────────
# (1) zero-copy를 쓴 케이스인데 성공률이 100%가 아니거나 기록이 없으면 비교에 쓸 수 없다 (지표 5)
zc_bad = (df["sink"] == "msg_zc") & (df["zc_success_pct"].isna() | (df["zc_success_pct"] < 99.99))
# (2) 실행 중 스로틀링이 발생한 행 (n/a = vcgencmd 없는 환경 → 판정 불가로 보고 유지)
thr = df["throttled"].fillna("n/a").astype(str).str.strip().str.lower()
thr_bad = ~thr.isin(["0x0", "n/a"])

for mask, why in ((zc_bad, "zero-copy 미성립(100% 미만 또는 기록 없음)"),
                  (thr_bad & ~zc_bad, "실행 중 스로틀링 발생")):
    if mask.any():
        print(f"[경고] {why} 실행 {int(mask.sum())}건을 제외합니다.")
        print(df.loc[mask, ["pass", "case", "tag", "zc_success_pct", "throttled"]].to_string(index=False))
df = df[~(zc_bad | thr_bad)]

# n/a = vcgencmd를 읽지 못한 실행 → 스로틀 판정이 빠졌다는 사실이 드러나도록 건수를 알린다
thr_na = int((thr[df.index] == "n/a").sum())
if thr_na:
    print(f"[경고] throttled=n/a 행 {thr_na}건 — 스로틀 판정 불가 상태로 포함됩니다. "
          "RPi5에서 일반 사용자로 `vcgencmd get_throttled`가 동작하는지 확인하세요 (보통 video 그룹 필요).")

if df.empty:
    sys.exit("유효한 데이터가 없습니다. 루프백이 아닌 실제 NIC 경로에서 측정하세요.")

# 완료 통보 누락 — 성공률이 "받은 통보" 기준이라 누락분의 폴백 여부는 알 수 없다
zc_missing = (df["sink"] == "msg_zc") & (df["zc_completed"] < df["zc_sent"])
if zc_missing.any():
    print(f"[경고] 완료 통보 누락 행 {int(zc_missing.sum())}건 (zc_completed < zc_sent) — "
          "성공률은 수신된 통보 기준입니다:")
    print(df.loc[zc_missing, ["pass", "tag", "zc_sent", "zc_completed"]].to_string(index=False))

# 전송 단위 라벨 (0 = 프레임 전체)
df["unit"] = df["chunk_bytes"].map(lambda b: "full" if b == 0 else f"{int(b) // 1024}K")
df["unit_bytes"] = df["chunk_bytes"].where(df["chunk_bytes"] > 0, float("inf"))
UNIT_ORDER = (df.groupby("unit")["unit_bytes"].max().sort_values().index.tolist())
df["ctx_total"] = df["ctx_vol"] + df["ctx_invol"]

cpu = df[df["pass"] == "cpu"]
copy = df[df["pass"] == "copy"]

KEYS = ["mtu", "fps", "case", "unit"]


def aggregate(sub, col):
    """조건(KEYS)별 평균·표준편차·표본 수·95% 신뢰구간 반폭."""
    if col not in sub.columns:
        return pd.DataFrame()
    s = sub.dropna(subset=[col])
    if s.empty:
        return pd.DataFrame()
    a = s.groupby(KEYS)[col].agg(["mean", "std", "count"]).reset_index()
    a["ci"] = [t95(int(n)) * sd / math.sqrt(n) if n >= 2 else float("nan")
               for sd, n in zip(a["std"], a["count"])]
    return a


def grid_bar(sub, col, ylabel, title, fname, lower_better=True):
    """MTU(행) × fps(열) 격자. 각 칸 = 전송 단위별 케이스 막대 + 95% CI 오차 막대."""
    a = aggregate(sub, col)
    if a.empty:
        print(f"[참고] {col} 데이터가 없어 {fname}을 건너뜁니다.")
        return
    mtus = sorted(a["mtu"].unique())
    fpss = sorted(a["fps"].unique())
    fig, axes = plt.subplots(len(mtus), len(fpss),
                             figsize=(4.8 * len(fpss), 3.8 * len(mtus)), squeeze=False)
    for i, mtu in enumerate(mtus):
        for j, fps in enumerate(fpss):
            ax = axes[i][j]
            cell = a[(a["mtu"] == mtu) & (a["fps"] == fps)]
            if cell.empty:
                ax.set_visible(False)
                continue
            order = [u for u in UNIT_ORDER if u in set(cell["unit"])]
            mean = cell.pivot(index="unit", columns="case", values="mean").reindex(order)
            ci = cell.pivot(index="unit", columns="case", values="ci").reindex(order)
            # n<2 칸은 신뢰구간을 계산할 수 없어 오차 막대 없이 그린다
            mean.plot(kind="bar", ax=ax, yerr=ci.fillna(0), capsize=3, rot=0)
            ax.set_title(f"MTU {mtu} / {fps} fps")
            ax.set_xlabel("send unit")
            ax.set_ylabel(ylabel)
            ax.grid(alpha=0.3, axis="y")
            ax.legend(fontsize=8)
    better = "lower" if lower_better else "higher"
    fig.suptitle(f"{title}  (bar = mean, error = 95% CI, {better} is better)")
    fig.tight_layout()
    fig.savefig(os.path.join(OUTDIR, fname), dpi=140)
    plt.close(fig)


# ── 0. 실제 달성 fps — 목표 fps 조건이 성립했는지 (cpu 패스) ───────
fps_ok = aggregate(cpu, "achieved_fps")
if not fps_ok.empty:
    short = fps_ok[fps_ok["mean"] < fps_ok["fps"] * 0.98]
    if not short.empty:
        print("[경고] 달성 fps가 목표의 98% 미만인 조건 — 링크 포화·버퍼 대기로 해당 fps 조건이 성립하지 않았습니다:")
        print(short[["mtu", "fps", "case", "unit", "mean"]]
              .rename(columns={"mean": "achieved_fps"}).to_string(index=False))
    grid_bar(cpu, "achieved_fps", "achieved fps", "Metric 0: achieved frame rate",
             "0_achieved_fps.png", lower_better=False)

# ── 1. %CPU per Gbps (층1 지표 1) — cpu 패스 ──────────────────────
if cpu.empty:
    print("[참고] cpu 패스 데이터가 없어 지표 1·4·5를 건너뜁니다.")
else:
    grid_bar(cpu, "cpu_per_gbps", "%CPU per Gbps", "Metric 1: CPU efficiency", "1_cpu_per_gbps.png")

# ── 1b. 시스템 전체 %CPU per Gbps (softirq·IRQ 포함, mpstat) — cpu 패스 ──
has_sys = "sys_cpu_per_gbps" in cpu.columns and cpu["sys_cpu_per_gbps"].notna().any()
if has_sys:
    grid_bar(cpu, "sys_cpu_per_gbps", "system %CPU per Gbps (1 core = 100%)",
             "Metric 1b: system-wide CPU efficiency (incl. softirq)", "1b_sys_cpu_per_gbps.png")
elif not cpu.empty:
    print("[참고] sys_cpu_per_gbps 값이 없어 지표 1b를 건너뜁니다 (merge_copy.py로 mpstat 병합 필요).")

# ── 2. 커널 복사 시간 (층1 지표 2) — copy 패스 ────────────────────
has_copy = "copy_us_frame" in copy.columns and copy["copy_us_frame"].notna().any()
if has_copy:
    grid_bar(copy, "copy_us_frame", "copy time per frame (us)",
             "Metric 2: kernel copy time per frame", "2_copy_us_per_frame.png")
    grid_bar(copy, "copy_time_pct", "copy time share of run (%)",
             "Metric 2b: kernel copy time share", "2b_copy_time_pct.png")
else:
    print("[참고] copy 패스의 copy_us_frame 값이 없어 지표 2·3 그래프를 건너뜁니다.")
    print("       ./scripts/run_matrix.sh <target> 3 60 copy")
    print("       python3 analysis/merge_copy.py data/summary.csv data/raw")

# ── 3. 복사 함수 호출 수 (층2 지표 3) — copy 패스 ─────────────────
if has_copy and "copy_calls" in copy.columns:
    grid_bar(copy, "copy_calls", "copy fn calls", "Metric 3: copy function calls", "3_copy_calls.png")

# ── 4. 컨텍스트 스위치 (층2 지표 4) — cpu 패스 ────────────────────
if not cpu.empty:
    grid_bar(cpu, "ctx_total", "context switches", "Metric 4: context switches", "4_ctx_switches.png")

# ── 5. G1 → G2 효과 분리 요약 — cpu 패스 ──────────────────────────


def gain_summary(col, label, fname):
    """G1 대비 G2 절감률 표(평균 ± 95% CI)와 선 그래프."""
    a = aggregate(cpu, col)
    if a.empty or not {"G1", "G2"}.issubset(set(a["case"])):
        return
    g1 = a[a["case"] == "G1"].set_index(["mtu", "fps", "unit"])
    g2 = a[a["case"] == "G2"].set_index(["mtu", "fps", "unit"])
    common = sorted(g1.index.intersection(g2.index),
                    key=lambda k: (k[0], k[1], UNIT_ORDER.index(k[2])))

    rows = []
    for key in common:
        m1, c1, n1 = g1.loc[key, "mean"], g1.loc[key, "ci"], int(g1.loc[key, "count"])
        m2, c2, n2 = g2.loc[key, "mean"], g2.loc[key, "ci"], int(g2.loc[key, "count"])
        red = (m1 - m2) / m1 * 100 if m1 else float("nan")
        if pd.isna(c1) or pd.isna(c2):
            verdict = "n<2 (판정 불가)"
        elif (m1 - c1) <= (m2 + c2) and (m2 - c2) <= (m1 + c1):
            verdict = "CI 겹침 — 차이 불확실"
        else:
            verdict = "CI 분리"
        rows.append((*key, m1, c1, n1, m2, c2, n2, red, verdict))

    print(f"\nG1 → G2 ({label}, 평균 ± 95% CI)")
    print(f"{'MTU':>5} {'fps':>4} {'unit':>5} | {'G1':>16} {'n':>3} | {'G2':>16} {'n':>3} | {'절감률':>7} | 판정")
    for mtu, fps, unit, m1, c1, n1, m2, c2, n2, red, verdict in rows:
        print(f"{mtu:>5} {fps:>4} {unit:>5} | {m1:>8.2f} ± {c1:>5.2f} {n1:>3} | "
              f"{m2:>8.2f} ± {c2:>5.2f} {n2:>3} | {red:>6.1f}% | {verdict}")
    print("  ※ 'CI 분리'는 두 신뢰구간이 겹치지 않는다는 뜻의 보수적 기준이다 (정식 검정 아님).")

    red_df = pd.DataFrame(rows, columns=["mtu", "fps", "unit", "m1", "c1", "n1",
                                         "m2", "c2", "n2", "reduction", "verdict"])
    fig, ax = plt.subplots(figsize=(8, 5))
    for (mtu, fps), g in red_df.groupby(["mtu", "fps"]):
        pos = [UNIT_ORDER.index(u) for u in g["unit"]]
        ax.plot(pos, g["reduction"], marker="o", label=f"MTU {mtu} / {fps} fps")
    ax.set_xticks(range(len(UNIT_ORDER)))
    ax.set_xticklabels(UNIT_ORDER)
    ax.axhline(0, color="gray", lw=0.8)
    ax.set_xlabel("send unit")
    ax.set_ylabel("CPU reduction (%)")
    ax.set_title(f"G1 -> G2: zero-copy net effect ({col})")
    ax.grid(alpha=0.3)
    ax.legend(fontsize=8)
    fig.tight_layout()
    fig.savefig(os.path.join(OUTDIR, fname), dpi=140)
    plt.close(fig)


gain_summary("cpu_per_gbps", "%CPU per Gbps — 프로세스", "5_zerocopy_gain.png")
if has_sys:
    # 프로세스 기준(5)과 시스템 기준(5b)의 절감률이 크게 다르면, 프로세스 밖(softirq 등)으로 옮겨간 비용이 있다는 뜻
    gain_summary("sys_cpu_per_gbps", "%CPU per Gbps — 시스템 전체", "5b_zerocopy_gain_sys.png")

print(f"\n그래프 생성 완료 → {OUTDIR}/")
