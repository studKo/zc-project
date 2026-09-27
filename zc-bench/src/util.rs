//! 시간·CPU 측정 유틸리티.

use std::io;

/// CLOCK_MONOTONIC_RAW 기준 나노초.
/// NTP 보정의 영향을 받지 않으므로 구간 측정에 적합하다.
pub fn now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC_RAW, &mut ts);
    }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

/// 지정 시각(now_ns 기준)까지 대기.
pub fn sleep_until(deadline_ns: u64) {
    let now = now_ns();
    if deadline_ns <= now {
        return;
    }
    let d = deadline_ns - now;
    let ts = libc::timespec {
        tv_sec: (d / 1_000_000_000) as libc::time_t,
        tv_nsec: (d % 1_000_000_000) as i64,
    };
    unsafe {
        let mut rem = std::mem::zeroed::<libc::timespec>();
        libc::nanosleep(&ts, &mut rem);
    }
}

/// 자기 프로세스의 CPU 사용 시간 (user + sys, 나노초).
/// `/proc/self/stat` 대신 getrusage를 쓴다 — 틱 해상도 문제를 피할 수 있다.
pub fn self_cpu_ns() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe {
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
    }
    let u = (ru.ru_utime.tv_sec as u64) * 1_000_000_000 + (ru.ru_utime.tv_usec as u64) * 1_000;
    let s = (ru.ru_stime.tv_sec as u64) * 1_000_000_000 + (ru.ru_stime.tv_usec as u64) * 1_000;
    u + s
}

/// 자발/비자발 컨텍스트 스위치 횟수.
pub fn self_ctx_switches() -> (u64, u64) {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe {
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
    }
    (ru.ru_nvcsw as u64, ru.ru_nivcsw as u64)
}

/// 정렬된 슬라이스에서 분위수 추출 (nearest-rank).
pub fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

pub fn mean(v: &[u64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.iter().map(|x| *x as f64).sum::<f64>() / v.len() as f64
}

pub fn stddev(v: &[u64]) -> f64 {
    if v.len() < 2 {
        return 0.0;
    }
    let m = mean(v);
    let var = v.iter().map(|x| (*x as f64 - m).powi(2)).sum::<f64>() / (v.len() - 1) as f64;
    var.sqrt()
}

/// RPi `vcgencmd get_throttled` 원시 값 (vcgencmd 미설치·실패 시 None).
///
/// - bit0~3  : 현재 상태 (저전압 / 클럭 제한 / 스로틀 / 온도 소프트 제한)
/// - bit16~19: 부팅 이후 한 번이라도 발생한 이력 — 한 번 켜지면 재부팅 전까지 유지된다
pub fn throttled_raw() -> Option<u32> {
    let out = std::process::Command::new("vcgencmd")
        .arg("get_throttled")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    // "throttled=0x50005"
    let v = s.trim().split('=').nth(1)?;
    u32::from_str_radix(v.trim().trim_start_matches("0x"), 16).ok()
}

/// 실행 구간의 스로틀링 판정값 (CSV `throttled` 컬럼, "0x0"이어야 유효).
///
/// 누적 비트(16~19)를 그대로 쓰면 한 번 스로틀된 뒤의 모든 실행이 비0이 되어
/// 실행별 판정이 불가능하다. 그래서
///   (종료 시점 현재 상태 bit0~3) | (이번 실행 중 새로 켜진 누적 비트를 bit0~3 위치로 내린 값)
/// 만 기록한다. 실행 전에 이미 켜져 있던 누적 비트는 이번 실행 중 재발했는지 알 수 없다는 한계가 있다.
pub fn throttled_during(start: Option<u32>, end: Option<u32>) -> String {
    match end {
        None => "n/a".into(),
        Some(e) => {
            let now = e & 0xF;
            let newly = ((e & !start.unwrap_or(0)) >> 16) & 0xF;
            format!("0x{:x}", now | newly)
        }
    }
}

/// SoC 온도(°C). sysfs thermal_zone0 → `vcgencmd measure_temp` 순으로 시도, 둘 다 실패하면 None.
pub fn temp_c() -> Option<f64> {
    if let Ok(s) = std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp") {
        if let Ok(milli) = s.trim().parse::<f64>() {
            return Some(milli / 1000.0);
        }
    }
    let out = std::process::Command::new("vcgencmd")
        .arg("measure_temp")
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    // "temp=48.3'C"
    s.trim().strip_prefix("temp=")?.trim_end_matches("'C").parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttled_ignores_sticky_bits_set_before_run() {
        // 부팅 후 이력(bit16)만 켜져 있고 이번 실행 중 변화 없음 → 유효
        assert_eq!(throttled_during(Some(0x10000), Some(0x10000)), "0x0");
        // 이번 실행 중 새로 스로틀 이력(bit18)이 생김 → bit2로 표시
        assert_eq!(throttled_during(Some(0x0), Some(0x40000)), "0x4");
        // 종료 시점 현재 상태 비트는 그대로 반영
        assert_eq!(throttled_during(Some(0x50000), Some(0x50005)), "0x5");
        assert_eq!(throttled_during(None, None), "n/a");
    }
}

pub fn last_os_error() -> io::Error {
    io::Error::last_os_error()
}
