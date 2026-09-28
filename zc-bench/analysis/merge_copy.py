#!/usr/bin/env python3
"""bpftrace 복사 시간·mpstat 시스템 CPU 측정 결과를 summary.csv에 병합한다.

지표 2(프레임당 커널 복사 시간)와 지표 3(복사 함수 호출 수)은 zc-bench 프로세스 밖의
bpftrace가, 지표 1의 교차 확인용 시스템 전체 CPU는 mpstat이 수집하므로, 실행 후 tag를
키로 결합한다. tag에 패스 이름(cpu/copy)이 들어가므로 복사 시간은 copy 패스 행에만 붙는다.

사용:
  python3 analysis/merge_copy.py data/summary.csv data/raw
  → data/summary_full.csv 생성 (cpu 패스만 돌렸어도 실행 — mpstat 병합)

추가되는 컬럼 — bpftrace (copy 패스)
  copy_ns_total   커널 복사 함수 체류 시간 총합 (ns, zc-bench 프로세스만)
  copy_calls      복사 함수 호출 횟수
  copy_us_frame   프레임당 복사 시간 (us) = copy_ns_total / frames_total      ← 지표 2 주 지표
  copy_time_pct   복사 시간 점유율 (%)   = copy_ns_total / run_ns × 100      ← 지표 2 보조
  bpf_wall_ns     bpftrace 부착 시간 (참고용)

추가되는 컬럼 — mpstat (모든 패스, 그래프는 cpu 패스만 사용)
  sys_cpu_pct       시스템 전체 CPU (%usr+%nice+%sys+%irq+%soft), 코어 1개 = 100% 단위
                    — zc-bench의 cpu_pct와 같은 단위라 직접 비교할 수 있다
  sys_soft_pct      그중 softirq 몫 (같은 단위)
  sys_cpu_per_gbps  sys_cpu_pct / (goodput_mbps / 1000)                       ← 지표 1b
  mpstat_samples    평균에 쓴 1초 표본 수

분모·구간 선택 이유
  bpftrace는 comm 필터로 zc-bench의 복사만 잡고, 워밍업을 포함한 실행 전체를 관측한다.
  그래서 분모도 워밍업 포함 전체 전송 프레임 수(frames_total)와 zc-bench 총 실행 시간(run_ns)을 쓴다.
  bpftrace의 wall_ns는 부착 대기와 timeout 여유분이 섞여 실제 실행 구간보다 길므로 분모로 쓰지 않는다.

  mpstat은 zc-bench 직전에 시작해 직후에 끝나므로, 마지막 2개 표본(flush·종료)을 버리고
  그 앞의 (elapsed_s - 1)개 표본을 측정 구간으로 본다 (1초 해상도의 근사).
  시스템 전체 값이라 다른 프로세스와 mpstat 자신의 부하도 포함된다 — copy 패스에서는 bpftrace도 포함.
"""
import os
import re
import sys

import pandas as pd

SUMMARY = sys.argv[1] if len(sys.argv) > 1 else "data/summary.csv"
RAWDIR = sys.argv[2] if len(sys.argv) > 2 else "data/raw"
OUT = os.path.join(os.path.dirname(SUMMARY) or ".", "summary_full.csv")

REQUIRED = {"tag", "pass", "frames", "frames_total", "run_ns", "elapsed_s", "goodput_mbps"}

# pandas 기본값은 "null"·"n/a"를 결측치로 읽는다 → sink="null", throttled="n/a"가 지워진 채
# summary_full.csv에 저장되므로, zc-bench가 쓰는 "NaN"과 빈 칸만 결측으로 본다.
READ_OPTS = dict(keep_default_na=False, na_values=["NaN", ""])

MPSTAT_TAIL_DROP = 2
BUSY_COLS = ("%usr", "%nice", "%sys", "%irq", "%soft")


def parse_copy_file(path):
    """bpftrace 출력에서 wall_ns / copy_ns_total / copy_calls 추출.

    END 블록이 실행되지 않았으면(wall_ns 없음 — 부착 실패나 강제 종료) None을 돌려준다.
    END가 실행됐는데 합계 줄이 없으면 복사 0건으로 본다.
    """
    with open(path, errors="replace") as f:
        txt = f.read()

    def grab(pattern):
        m = re.search(pattern, txt, re.MULTILINE)
        return int(m.group(1)) if m else None

    wall = grab(r"^wall_ns\s+(\d+)")
    if wall is None:
        return None
    return {
        "wall_ns": wall,
        "copy_ns_total": grab(r"^@copy_ns_total:\s*(\d+)") or 0,
        "copy_calls": grab(r"^@copy_calls:\s*(\d+)") or 0,
    }


def parse_mpstat(path):
    """`mpstat -P ALL 1` 출력에서 코어 수와 'all' 행의 초당 (busy%, soft%) 표본을 읽는다.

    열 위치는 각 블록의 헤더 줄(CPU, %usr, ...)에서 찾는다 — 시각 표기(AM/PM 유무)에 따라
    앞쪽 토큰 수가 달라지기 때문이다. 'Average:' 줄은 버린다.
    """
    ncpu, idx, samples = None, None, []
    with open(path, errors="replace") as f:
        for line in f:
            tok = line.split()
            if not tok:
                continue
            if ncpu is None:
                m = re.search(r"\((\d+) CPU\)", line)
                if m:
                    ncpu = int(m.group(1))
                    continue
            if tok[0].startswith("Average"):
                continue
            if "CPU" in tok and "%usr" in tok:
                idx = {name: i for i, name in enumerate(tok)}
                continue
            if idx is None or len(tok) != len(idx) or tok[idx["CPU"]] != "all":
                continue
            try:
                busy = sum(float(tok[idx[c]]) for c in BUSY_COLS if c in idx)
                soft = float(tok[idx["%soft"]]) if "%soft" in idx else float("nan")
            except ValueError:
                continue
            samples.append((busy, soft))
    return ncpu, samples


def mpstat_window(ncpu, samples, elapsed_s):
    """측정 구간에 해당하는 표본 평균 → (sys_cpu_pct, sys_soft_pct, 표본 수). 코어 1개 = 100% 단위."""
    if not ncpu or not samples:
        return None
    body = samples[:-MPSTAT_TAIL_DROP] if len(samples) > MPSTAT_TAIL_DROP else samples
    win = max(1, int(elapsed_s) - 1)
    body = body[-win:]
    busy = sum(b for b, _ in body) / len(body)
    soft = sum(s for _, s in body) / len(body)
    return busy * ncpu, soft * ncpu, len(body)


def merge_copy(df, is_copy):
    rows, broken = {}, []
    for fn in sorted(os.listdir(RAWDIR)):
        if not fn.endswith(".copy"):
            continue
        tag = fn[: -len(".copy")]
        parsed = parse_copy_file(os.path.join(RAWDIR, fn))
        if parsed is None:
            broken.append(tag)
        else:
            rows[tag] = parsed

    if broken:
        print(f"[경고] END 출력이 없는 .copy 파일 {len(broken)}건 (bpftrace 부착 실패/강제 종료) — 병합에서 제외:")
        for t in broken[:10]:
            print(f"    {t}   → {RAWDIR}/{t}.copy.err 확인")
        if len(broken) > 10:
            print(f"    ... 외 {len(broken) - 10}건")

    def col(key):
        vals = df["tag"].map(lambda t: rows.get(t, {}).get(key)).where(is_copy)
        return pd.to_numeric(vals, errors="coerce")

    df["copy_ns_total"] = col("copy_ns_total")
    df["copy_calls"] = col("copy_calls")
    df["bpf_wall_ns"] = col("wall_ns")

    frames_total = df["frames_total"].where(df["frames_total"] > 0)
    run_ns = df["run_ns"].where(df["run_ns"] > 0)
    df["copy_us_frame"] = (df["copy_ns_total"] / frames_total / 1000).round(3)
    df["copy_time_pct"] = (df["copy_ns_total"] / run_ns * 100).round(4)

    n_copy = int(is_copy.sum())
    matched = int(df["copy_ns_total"].notna().sum())
    print(f"복사 시간: copy 패스 {n_copy}행 중 {matched}행에 결합")

    dup = df.loc[is_copy, "tag"].duplicated().sum()
    if dup:
        print(f"[경고] copy 패스 tag 중복 {dup}건 — 같은 tag의 .copy 값이 여러 행에 붙었습니다.")
    if n_copy and matched < n_copy:
        print(f"[참고] .copy 파일이 없거나 깨진 copy 패스 행 {n_copy - matched}건은 복사 시간이 비어 있습니다.")
    orphan = sorted(set(rows) - set(df.loc[is_copy, "tag"]))
    if orphan:
        print(f"[참고] summary에 대응 행이 없는 .copy 파일 {len(orphan)}건 (예: {orphan[0]})")
    if n_copy and matched == 0:
        print("[경고] copy 패스 tag가 하나도 일치하지 않았습니다. .copy 파일명과 --tag 값을 확인하세요.")
    return matched


def merge_mpstat(df):
    vals = {"sys_cpu_pct": [], "sys_soft_pct": [], "mpstat_samples": []}
    for tag, elapsed_s in zip(df["tag"], df["elapsed_s"]):
        path = os.path.join(RAWDIR, f"{tag}.mpstat")
        res = None
        if os.path.exists(path) and pd.notna(elapsed_s):
            res = mpstat_window(*parse_mpstat(path), elapsed_s)
        busy, soft, n = res if res else (float("nan"), float("nan"), float("nan"))
        vals["sys_cpu_pct"].append(busy)
        vals["sys_soft_pct"].append(soft)
        vals["mpstat_samples"].append(n)

    for k, v in vals.items():
        df[k] = pd.Series(v, index=df.index)
    df["sys_cpu_pct"] = df["sys_cpu_pct"].round(2)
    df["sys_soft_pct"] = df["sys_soft_pct"].round(2)
    gbps = (df["goodput_mbps"] / 1000).where(df["goodput_mbps"] > 0)
    df["sys_cpu_per_gbps"] = (df["sys_cpu_pct"] / gbps).round(2)

    matched = int(df["sys_cpu_pct"].notna().sum())
    print(f"mpstat: {len(df)}행 중 {matched}행에 시스템 CPU 결합")
    return matched


def main():
    if not os.path.exists(SUMMARY):
        sys.exit(f"summary 파일이 없습니다: {SUMMARY}")
    if not os.path.isdir(RAWDIR):
        sys.exit(f"raw 디렉터리가 없습니다: {RAWDIR}")

    df = pd.read_csv(SUMMARY, **READ_OPTS)
    missing = REQUIRED - set(df.columns)
    if missing:
        sys.exit(f"{SUMMARY}에 {sorted(missing)} 컬럼이 없습니다 — 이전 스키마 파일입니다. "
                 "새 zc-bench로 다시 측정하세요.")

    is_copy = df["pass"] == "copy"
    n_copy_matched = merge_copy(df, is_copy)
    n_mp_matched = merge_mpstat(df)

    if n_copy_matched == 0 and n_mp_matched == 0:
        sys.exit(f"{RAWDIR}에서 병합할 .copy / .mpstat 결과를 찾지 못했습니다.")

    df.to_csv(OUT, index=False)
    print(f"병합 완료 → {OUT}  (전체 {len(df)}행)")


if __name__ == "__main__":
    main()
