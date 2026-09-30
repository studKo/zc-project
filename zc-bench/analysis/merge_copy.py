#!/usr/bin/env python3
"""bpftrace 측정 결과(복사 점유율·송신 호출)와 mpstat 시스템 CPU를 summary.csv에 병합한다.

지표 2(커널 복사 시간)와 지표 3(송신 호출 수)은 zc-bench 프로세스 밖의 bpftrace가,
지표 1의 교차 확인용 시스템 전체 CPU는 mpstat이 수집하므로, 실행 후 tag를 키로 결합한다.
tag에 패스 이름(cpu/copy)이 들어가므로 bpftrace 값은 copy 패스 행에만 붙는다.

사용:
  python3 analysis/merge_copy.py data/summary.csv data/raw
  → data/summary_full.csv 생성 (cpu 패스만 돌렸어도 실행 — mpstat 병합)

지표 2·3 측정 방식 (scripts/measure_copy.bt 머리말 참고)
  RPi5 커널(6.8.0-1051-raspi)에서는 복사 함수(_copy_from_iter, __arch_copy_from_user 등)에
  kprobe를 걸 수 없어, 함수를 직접 프로브하는 대신 997 Hz 샘플링으로 "zc-bench가 커널의
  __arch_copy_from_user 안에 있던 표본의 비율"을 센다. 통계적 추정치이며 표본 수가 적으면 오차가 크다.
  지표 3은 복사 함수 호출 수 대신 kprobe가 붙는 tcp_sendmsg_locked 호출 수를 쓴다.

추가되는 컬럼 — bpftrace (copy 패스)
  bpf_samples     zc-bench가 CPU 위에 있던 전체 표본 수 (신뢰도 판단용)
  copy_samples    그중 유저→커널 복사 함수(COPY_SYMS)에 있던 표본 수
  copy_ns_est     추정 복사 시간 (ns) = copy_samples / hz × 1e9
  copy_cpu_pct    프로세스 CPU 시간 중 복사 비율 (%) = copy_samples / bpf_samples × 100
  copy_us_frame   프레임당 추정 복사 시간 (us) = copy_ns_est / frames_total / 1000   ← 지표 2 주 지표
  copy_time_pct   실행 시간 대비 복사 시간 (%) = copy_ns_est / run_ns × 100           ← 지표 2 보조
  send_calls      tcp_sendmsg_locked 호출 수                                          ← 지표 3
  send_us_frame   프레임당 tcp_sendmsg_locked 체류 시간 (us)                          ← 지표 2 보조
                  (복사 외에 skb 할당 등 송신 처리 전체가 포함되므로 "복사 시간"이 아니라 "송신 함수 시간")
  bpf_wall_ns     bpftrace 부착 시간 (참고용)

추가되는 컬럼 — mpstat (모든 패스, 그래프는 cpu 패스만 사용)
  sys_cpu_pct       시스템 전체 CPU (%usr+%nice+%sys+%irq+%soft), 코어 1개 = 100% 단위
                    — zc-bench의 cpu_pct와 같은 단위라 직접 비교할 수 있다
  sys_soft_pct      그중 softirq 몫 (같은 단위)
  sys_cpu_per_gbps  sys_cpu_pct / (goodput_mbps / 1000)                       ← 지표 1b
  mpstat_samples    평균에 쓴 1초 표본 수

분모·구간 선택 이유
  bpftrace는 comm 필터로 zc-bench만 보고, 워밍업을 포함한 실행 전체를 관측한다.
  그래서 분모도 워밍업 포함 전체 전송 프레임 수(frames_total)와 zc-bench 총 실행 시간(run_ns)을 쓴다.
  bpftrace의 wall_ns는 부착 대기와 timeout 여유분이 섞여 실제 실행 구간보다 길므로 분모로 쓰지 않는다.
  루프백에서는 수신기(comm도 zc-bench)의 __arch_copy_to_user 표본이 bpf_samples에 섞이지만,
  복사 방향이 달라 copy_samples에는 들어가지 않는다 (분모만 커져 copy_cpu_pct가 낮게 나온다).

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

# 유저→커널 복사 함수. 이 목록에 든 커널 함수에서 잡힌 표본을 "복사 중"으로 센다.
# (__arch_copy_to_user 는 반대 방향이라 제외)
COPY_SYMS = ("__arch_copy_from_user", "_copy_from_iter", "_copy_from_iter_nocache", "copy_page_from_iter")

# 표본이 이보다 적으면 복사 점유율의 오차가 크다는 경고를 낸다 (997 Hz × CPU 점유 시간)
MIN_SAMPLES = 200

MPSTAT_TAIL_DROP = 2
BUSY_COLS = ("%usr", "%nice", "%sys", "%irq", "%soft")


def parse_copy_file(path):
    """bpftrace 출력에서 wall_ns / hz / 표본 수 / 송신 호출 / 복사 표본을 추출한다.

    END 블록이 실행되지 않았으면(wall_ns·hz 없음 — 부착 실패나 강제 종료) None을 돌려준다.
    @top 은 키가 주소 단위라 같은 함수 이름이 여러 줄로 나오므로 이름별로 합산한다.
    """
    with open(path, errors="replace") as f:
        txt = f.read()

    def grab(pattern):
        m = re.search(pattern, txt, re.MULTILINE)
        return int(m.group(1)) if m else None

    wall = grab(r"^wall_ns\s+(\d+)")
    hz = grab(r"^hz\s+(\d+)")
    if wall is None or hz is None:
        return None

    copy_samples = 0
    for m in re.finditer(r"^@top\[(.+?)\]:\s*(\d+)\s*$", txt, re.MULTILINE):
        if m.group(1) in COPY_SYMS:
            copy_samples += int(m.group(2))

    return {
        "wall_ns": wall,
        "hz": hz,
        "bpf_samples": grab(r"^@samples_total:\s*(\d+)") or 0,
        "copy_samples": copy_samples,
        "send_ns_total": grab(r"^@tsl_ns:\s*(\d+)") or 0,
        "send_calls": grab(r"^@tsl_calls:\s*(\d+)") or 0,
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
        print(f"[경고] END 출력이 없거나 이전 형식인 .copy 파일 {len(broken)}건 "
              "(bpftrace 부착 실패/강제 종료/구 스크립트) — 병합에서 제외:")
        for t in broken[:10]:
            print(f"    {t}   → {RAWDIR}/{t}.copy.err 확인")
        if len(broken) > 10:
            print(f"    ... 외 {len(broken) - 10}건")

    def col(key):
        vals = df["tag"].map(lambda t: rows.get(t, {}).get(key)).where(is_copy)
        return pd.to_numeric(vals, errors="coerce")

    df["bpf_samples"] = col("bpf_samples")
    df["copy_samples"] = col("copy_samples")
    df["send_calls"] = col("send_calls")
    df["bpf_wall_ns"] = col("wall_ns")
    hz = col("hz")
    send_ns = col("send_ns_total")

    frames_total = df["frames_total"].where(df["frames_total"] > 0)
    run_ns = df["run_ns"].where(df["run_ns"] > 0)
    df["copy_ns_est"] = (df["copy_samples"] / hz * 1e9).round(0)
    df["copy_cpu_pct"] = (df["copy_samples"] / df["bpf_samples"].where(df["bpf_samples"] > 0) * 100).round(2)
    df["copy_us_frame"] = (df["copy_ns_est"] / frames_total / 1000).round(3)
    df["copy_time_pct"] = (df["copy_ns_est"] / run_ns * 100).round(4)
    df["send_us_frame"] = (send_ns / frames_total / 1000).round(3)

    n_copy = int(is_copy.sum())
    matched = int(df["copy_samples"].notna().sum())
    print(f"복사 점유율·송신 호출: copy 패스 {n_copy}행 중 {matched}행에 결합")

    dup = df.loc[is_copy, "tag"].duplicated().sum()
    if dup:
        print(f"[경고] copy 패스 tag 중복 {dup}건 — 같은 tag의 .copy 값이 여러 행에 붙었습니다.")
    if n_copy and matched < n_copy:
        print(f"[참고] .copy 파일이 없거나 깨진 copy 패스 행 {n_copy - matched}건은 값이 비어 있습니다.")
    orphan = sorted(set(rows) - set(df.loc[is_copy, "tag"]))
    if orphan:
        print(f"[참고] summary에 대응 행이 없는 .copy 파일 {len(orphan)}건 (예: {orphan[0]})")
    if n_copy and matched == 0:
        print("[경고] copy 패스 tag가 하나도 일치하지 않았습니다. .copy 파일명과 --tag 값을 확인하세요.")

    few = df.loc[is_copy & df["bpf_samples"].notna() & (df["bpf_samples"] < MIN_SAMPLES), ["tag", "bpf_samples"]]
    if len(few):
        print(f"[경고] 표본 수가 {MIN_SAMPLES}개 미만인 행 {len(few)}건 — 샘플링 추정 오차가 큽니다 "
              "(측정 시간을 늘리거나 반복 횟수를 늘리세요):")
        print(few.head(5).to_string(index=False))
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
