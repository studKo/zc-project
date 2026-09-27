//! 측정 결과 집계 및 CSV 출력.

use crate::util::{
    mean, now_ns, percentile, self_cpu_ns, self_ctx_switches, stddev, temp_c, throttled_during,
    throttled_raw,
};
use crate::zcstat::ZcStats;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;

/// 프레임 단위 원시 기록.
#[derive(Debug, Clone, Copy)]
pub struct FrameRecord {
    pub seq: u64,
    pub tx_ns: u64,
    pub echo_ns: u64,
    pub latency_ns: u64,
    pub bytes: u64,
}

/// 실행 조건. 위치 인자 대신 이름 있는 필드로 넘겨 인자 순서 실수를 막는다.
pub struct RunConfig<'a> {
    pub case: &'a str,
    pub tag: &'a str,
    /// 측정 패스 라벨 (cpu | copy)
    pub pass: &'a str,
    pub sink: &'a str,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// 기록용 MTU 라벨 (0 = 미지정)
    pub mtu: u32,
    /// 1회 전송 단위 바이트 (0 = 프레임 전체)
    pub chunk_bytes: usize,
    pub warmup_s: u64,
    /// 송신 프로세스 시작 시각 (now_ns). `run_ns`의 기준점.
    pub proc_start_ns: u64,
}

/// 특정 시점의 시간·CPU·컨텍스트 스위치 스냅샷.
#[derive(Debug, Clone, Copy)]
struct Snapshot {
    ns: u64,
    cpu_ns: u64,
    ctx: (u64, u64),
}

impl Snapshot {
    fn take() -> Self {
        Self {
            ns: now_ns(),
            cpu_ns: self_cpu_ns(),
            ctx: self_ctx_switches(),
        }
    }
}

pub struct Run {
    pub case: String,
    pub tag: String,
    pub pass: String,
    pub sink: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub mtu: u32,
    pub chunk_bytes: usize,
    pub records: Vec<FrameRecord>,
    pub dropped: u64,
    start_ns: u64,
    warmup_ns: u64,
    proc_start_ns: u64,
    /// 측정 구간 시작 (워밍업 종료 시점)
    meas_start: Option<Snapshot>,
    /// 측정 구간 끝 (송신 루프 종료 직후, flush 이전)
    meas_end: Option<Snapshot>,
    /// flush까지 끝난 시각 (`run_ns` 끝점)
    finish_ns: Option<u64>,
    throttled_start: Option<u32>,
}

impl Run {
    pub fn new(cfg: &RunConfig) -> Self {
        // vcgencmd 호출은 수 ms 걸리므로 워밍업 기준 시각보다 먼저 끝낸다.
        let throttled_start = throttled_raw();
        Self {
            case: cfg.case.to_string(),
            tag: cfg.tag.to_string(),
            pass: cfg.pass.to_string(),
            sink: cfg.sink.to_string(),
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            mtu: cfg.mtu,
            chunk_bytes: cfg.chunk_bytes,
            records: Vec::with_capacity(8192),
            dropped: 0,
            start_ns: now_ns(),
            warmup_ns: cfg.warmup_s * 1_000_000_000,
            proc_start_ns: cfg.proc_start_ns,
            meas_start: None,
            meas_end: None,
            finish_ns: None,
            throttled_start,
        }
    }

    /// 송신 루프마다 호출. 워밍업이 끝나는 첫 시점에 CPU·컨텍스트 스위치 기준점을 찍는다.
    /// 이렇게 해야 %CPU의 분자(CPU 시간)와 분모(경과 시간)가 같은 구간을 가리킨다.
    pub fn tick(&mut self, now: u64) {
        if self.meas_start.is_none() && now >= self.start_ns + self.warmup_ns {
            self.meas_start = Some(Snapshot::take());
        }
    }

    /// 송신 루프 종료 직후(flush 이전) 호출 — 측정 구간의 끝.
    pub fn end_measure(&mut self) {
        let s = Snapshot::take();
        if self.meas_start.is_none() {
            // 워밍업이 실행 시간보다 길었던 경우: 측정 구간 0
            self.meas_start = Some(s);
        }
        self.meas_end = Some(s);
    }

    /// flush까지 끝난 직후 호출 — `run_ns`(총 실행 시간)의 끝점.
    pub fn finish(&mut self) {
        self.finish_ns = Some(now_ns());
    }

    pub fn push(&mut self, r: FrameRecord) {
        self.records.push(r);
    }

    pub fn summary(&self, zc: &ZcStats) -> Summary {
        let end = self.meas_end.unwrap_or_else(Snapshot::take);
        let start = self.meas_start.unwrap_or(end);

        // 워밍업을 제외한 기록
        let eff: Vec<&FrameRecord> = self.records.iter().filter(|r| r.tx_ns >= start.ns).collect();
        let n = eff.len() as u64;

        let elapsed_ns = end.ns.saturating_sub(start.ns);
        let elapsed_s = (elapsed_ns as f64 / 1e9).max(1e-9);

        let bytes: u64 = eff.iter().map(|r| r.bytes).sum();
        let goodput_mbps = (bytes as f64 * 8.0) / elapsed_s / 1e6;

        let cpu_ns = end.cpu_ns.saturating_sub(start.cpu_ns);
        let cpu_pct = (cpu_ns as f64 / elapsed_ns.max(1) as f64) * 100.0;
        let cpu_per_gbps = if goodput_mbps > 0.0 {
            cpu_pct / (goodput_mbps / 1000.0)
        } else {
            f64::NAN
        };

        let mut lat: Vec<u64> = eff
            .iter()
            .filter(|r| r.latency_ns > 0)
            .map(|r| r.latency_ns)
            .collect();
        lat.sort_unstable();

        let run_ns = self
            .finish_ns
            .unwrap_or_else(now_ns)
            .saturating_sub(self.proc_start_ns);

        Summary {
            case: self.case.clone(),
            tag: self.tag.clone(),
            pass: self.pass.clone(),
            sink: self.sink.clone(),
            width: self.width,
            height: self.height,
            fps: self.fps,
            mtu: self.mtu,
            chunk_bytes: self.chunk_bytes,
            frames: n,
            frames_total: self.records.len() as u64,
            elapsed_s,
            run_ns,
            goodput_mbps,
            cpu_pct,
            cpu_per_gbps,
            lat_p50_us: percentile(&lat, 50.0) as f64 / 1000.0,
            lat_p95_us: percentile(&lat, 95.0) as f64 / 1000.0,
            lat_p99_us: percentile(&lat, 99.0) as f64 / 1000.0,
            lat_max_us: lat.last().copied().unwrap_or(0) as f64 / 1000.0,
            lat_mean_us: mean(&lat) / 1000.0,
            lat_stddev_us: stddev(&lat) / 1000.0,
            zc_completed: zc.completed,
            zc_copied: zc.copied,
            zc_success_pct: zc.success_pct(),
            ctx_vol: end.ctx.0.saturating_sub(start.ctx.0),
            ctx_invol: end.ctx.1.saturating_sub(start.ctx.1),
            dropped: self.dropped,
            throttled: throttled_during(self.throttled_start, throttled_raw()),
            temp_c: temp_c().unwrap_or(f64::NAN),
        }
    }

    /// 프레임 단위 원시 CSV.
    pub fn write_raw_csv<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        let mut w = BufWriter::new(File::create(path)?);
        writeln!(w, "seq,tx_ns,echo_ns,latency_ns,bytes")?;
        for r in &self.records {
            writeln!(
                w,
                "{},{},{},{},{}",
                r.seq, r.tx_ns, r.echo_ns, r.latency_ns, r.bytes
            )?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub case: String,
    pub tag: String,
    pub pass: String,
    pub sink: String,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub mtu: u32,
    pub chunk_bytes: usize,
    /// 워밍업 제외 프레임 수
    pub frames: u64,
    /// 워밍업 포함 전체 전송 프레임 수 (bpftrace 복사 시간의 분모)
    pub frames_total: u64,
    /// 측정 구간 길이 (워밍업 종료 ~ 송신 루프 종료)
    pub elapsed_s: f64,
    /// zc-bench 총 실행 시간 (프로세스 시작 ~ flush 종료, ns)
    pub run_ns: u64,
    pub goodput_mbps: f64,
    pub cpu_pct: f64,
    pub cpu_per_gbps: f64,
    pub lat_p50_us: f64,
    pub lat_p95_us: f64,
    pub lat_p99_us: f64,
    pub lat_max_us: f64,
    pub lat_mean_us: f64,
    pub lat_stddev_us: f64,
    pub zc_completed: u64,
    pub zc_copied: u64,
    pub zc_success_pct: f64,
    pub ctx_vol: u64,
    pub ctx_invol: u64,
    pub dropped: u64,
    pub throttled: String,
    /// 실행 종료 시점 SoC 온도 (°C, 측정 불가 시 NaN)
    pub temp_c: f64,
}

pub const SUMMARY_HEADER: &str = "case,tag,pass,sink,width,height,fps,mtu,chunk_bytes,\
frames,frames_total,elapsed_s,run_ns,goodput_mbps,cpu_pct,cpu_per_gbps,\
lat_p50_us,lat_p95_us,lat_p99_us,lat_max_us,lat_mean_us,lat_stddev_us,\
zc_completed,zc_copied,zc_success_pct,ctx_vol,ctx_invol,dropped,throttled,temp_c";

impl Summary {
    pub fn csv_row(&self) -> String {
        format!(
            "{},{},{},{},{},{},{},{},{},\
             {},{},{:.3},{},{:.2},{:.2},{:.2},\
             {:.1},{:.1},{:.1},{:.1},{:.1},{:.1},\
             {},{},{:.2},{},{},{},{},{:.1}",
            self.case, self.tag, self.pass, self.sink, self.width, self.height, self.fps,
            self.mtu, self.chunk_bytes,
            self.frames, self.frames_total, self.elapsed_s, self.run_ns, self.goodput_mbps,
            self.cpu_pct, self.cpu_per_gbps,
            self.lat_p50_us, self.lat_p95_us, self.lat_p99_us, self.lat_max_us,
            self.lat_mean_us, self.lat_stddev_us,
            self.zc_completed, self.zc_copied, self.zc_success_pct,
            self.ctx_vol, self.ctx_invol, self.dropped, self.throttled, self.temp_c
        )
    }

    /// 요약 CSV에 한 행을 추가한다.
    ///
    /// 헤더는 파일을 만들 때 한 번만 쓰이므로, 기존 파일의 헤더가 현재 스키마와 다르면
    /// 행이 어긋난 채 섞이지 않도록 에러로 중단한다.
    ///
    /// 헤더를 새로 쓰는 경우는 파일이 없거나 0바이트일 때뿐이다. 비어 있지 않은데 첫 줄이
    /// 빈 줄이면(`echo > summary.csv` 등) 헤더로 인정하지 않고 에러로 처리한다 —
    /// 그러지 않으면 실행마다 헤더가 중간에 반복 삽입된다.
    pub fn append_csv<P: AsRef<Path>>(&self, path: P) -> io::Result<()> {
        let p = path.as_ref();
        let has_header = match File::open(p) {
            Ok(f) => {
                if f.metadata()?.len() == 0 {
                    false
                } else {
                    let mut first = String::new();
                    BufReader::new(f).read_line(&mut first)?;
                    if first.trim_end_matches(['\r', '\n']) != SUMMARY_HEADER {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!(
                                "{0}의 첫 줄이 현재 스키마 헤더와 다릅니다 (이전 버전 파일이거나 첫 줄이 비어 있음).\n  \
                                 기존 파일을 백업하거나 삭제한 뒤 다시 실행하세요: mv {0} {0}.old",
                                p.display()
                            ),
                        ));
                    }
                    true
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => return Err(e),
        };

        let mut f = OpenOptions::new().create(true).append(true).open(p)?;
        if !has_header {
            writeln!(f, "{}", SUMMARY_HEADER)?;
        }
        writeln!(f, "{}", self.csv_row())
    }

    pub fn print(&self) {
        println!(
            "\n══════════ RESULT: {} / {} (pass={}, mtu={}) ══════════",
            self.case, self.sink, self.pass, self.mtu
        );
        println!("  frames            : {} ({:.1}s, 전체 {})", self.frames, self.elapsed_s, self.frames_total);
        println!("  goodput           : {:.1} Mbps", self.goodput_mbps);
        println!("  CPU               : {:.2} %", self.cpu_pct);
        println!("  CPU per Gbps      : {:.2} %/Gbps   ← 1순위 지표", self.cpu_per_gbps);
        if self.lat_p50_us > 0.0 {
            println!(
                "  echo RTT/2 p50/p95/p99: {:.1} / {:.1} / {:.1} us",
                self.lat_p50_us, self.lat_p95_us, self.lat_p99_us
            );
            println!("  echo RTT/2 max/stddev : {:.1} / {:.1} us", self.lat_max_us, self.lat_stddev_us);
        }
        println!("  ctx switches      : vol {} / invol {}", self.ctx_vol, self.ctx_invol);
        println!("  dropped frames    : {}", self.dropped);

        if self.zc_completed > 0 {
            let verdict = if self.zc_success_pct >= 99.99 {
                "OK — 실제 zero-copy 성립"
            } else if self.zc_success_pct <= 0.01 {
                "FALLBACK — 커널이 전부 복사함 (루프백이면 정상)"
            } else {
                "PARTIAL — 일부만 zero-copy. 원인 규명 필요"
            };
            println!(
                "  zero-copy         : {:.2}% ({}/{} 건)  [{}]",
                self.zc_success_pct,
                self.zc_completed - self.zc_copied,
                self.zc_completed,
                verdict
            );
        }
        println!("  throttled         : {}   (실행 중 발생분, 0x0이어야 유효)", self.throttled);
        println!("  temp              : {:.1} °C", self.temp_c);
        println!("════════════════════════════════════════\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(proc_start_ns: u64) -> RunConfig<'static> {
        RunConfig {
            case: "G1",
            tag: "cpu_G1_test",
            pass: "cpu",
            sink: "copy",
            width: 1280,
            height: 960,
            fps: 30,
            mtu: 1500,
            chunk_bytes: 65536,
            warmup_s: 0,
            proc_start_ns,
        }
    }

    #[test]
    fn csv_row_matches_header_columns() {
        let mut run = Run::new(&cfg(now_ns()));
        run.tick(now_ns());
        run.end_measure();
        run.finish();
        let s = run.summary(&ZcStats::default());
        assert_eq!(
            s.csv_row().split(',').count(),
            SUMMARY_HEADER.split(',').count()
        );
        // 인자 순서 실수(#1) 회귀 방지: 청크 크기가 그대로 기록되어야 한다
        assert_eq!(s.chunk_bytes, 65536);
    }

    #[test]
    fn append_csv_rejects_old_schema() {
        let dir = std::env::temp_dir().join(format!("zc-bench-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("old.csv");
        std::fs::write(&p, "case,tag,sink\nG1,x,copy\n").unwrap();

        let mut run = Run::new(&cfg(now_ns()));
        run.end_measure();
        run.finish();
        let err = run.summary(&ZcStats::default()).append_csv(&p).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        // 비어 있지 않은데 첫 줄이 빈 줄인 파일도 거부해야 한다 (헤더 반복 삽입 방지)
        let blank = dir.join("blank.csv");
        std::fs::write(&blank, "\n").unwrap();
        let err = run.summary(&ZcStats::default()).append_csv(&blank).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        // 0바이트 파일은 새 파일처럼 헤더를 쓴다
        let empty = dir.join("empty.csv");
        std::fs::write(&empty, "").unwrap();
        run.summary(&ZcStats::default()).append_csv(&empty).unwrap();
        assert_eq!(std::fs::read_to_string(&empty).unwrap().lines().next(), Some(SUMMARY_HEADER));

        let q = dir.join("new.csv");
        let _ = std::fs::remove_file(&q);
        let s = run.summary(&ZcStats::default());
        s.append_csv(&q).unwrap();
        s.append_csv(&q).unwrap();
        let body = std::fs::read_to_string(&q).unwrap();
        assert_eq!(body.lines().count(), 3); // 헤더 1 + 행 2
        let _ = std::fs::remove_dir_all(&dir);
    }
}
