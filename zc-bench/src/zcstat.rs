//! MSG_ZEROCOPY 완료 알림 처리 및 버퍼 생명주기 관리.
//!
//! # 이 모듈이 존재하는 이유
//!
//! 일반 `send()`는 반환 시점에 커널이 이미 데이터를 복사했으므로 버퍼를
//! 즉시 재사용할 수 있다. 반면 `MSG_ZEROCOPY`는 커널이 **유저 페이지를 직접
//! 참조**하므로, NIC가 실제로 읽어갈 때까지 해당 버퍼를 수정하면 안 된다.
//!
//! 완료 여부는 `MSG_ERRQUEUE`로 비동기 통보되며, 이를 수거해 버퍼를 회수하는
//! 것이 이 모듈의 역할이다.
//!
//! # 무음 폴백 검출
//!
//! `MSG_ZEROCOPY`는 전제조건(scatter-gather 미지원 등)이 맞지 않으면
//! **에러 없이 조용히 복사로 되돌아간다.** 완료 알림의
//! `ee_code & SO_EE_CODE_ZEROCOPY_COPIED` 비트가 이를 알려주는 유일한 수단이다.
//!
//! 이 값을 집계하지 않으면 "zero-copy가 동작하지 않았음"을 "효과가 없음"으로
//! 오독하게 된다. 따라서 모든 실험에서 `success_pct()`를 필수 기록한다.
//!
//! 정밀도 한계: 커널은 연이은 완료 통보를 id 범위 하나로 합치며, 이때 COPIED 비트는
//! 범위 단위로 붙는다. errqueue를 프레임당 1회 수거하므로 범위가 프레임 하나 분량까지
//! 길어질 수 있다. 전부 성공(100%)·전부 폴백(0%) 판정에는 영향이 없고, 일부만 폴백한
//! PARTIAL 경우의 성공률만 범위 단위 근사치가 된다.

use std::collections::{HashMap, VecDeque};
use std::os::unix::io::RawFd;

// asm-generic 기준 상수 (aarch64/x86_64 공통)
pub const SO_ZEROCOPY: libc::c_int = 60;
pub const MSG_ZEROCOPY: libc::c_int = 0x0400_0000;
pub const SO_EE_ORIGIN_ZEROCOPY: u8 = 5;
pub const SO_EE_CODE_ZEROCOPY_COPIED: u8 = 1;
pub const IP_RECVERR: libc::c_int = 11;
pub const IPV6_RECVERR: libc::c_int = 25;

/// zero-copy 통보 집계.
#[derive(Debug, Default, Clone)]
pub struct ZcStats {
    /// MSG_ZEROCOPY로 보낸 전송 건수 (성공한 sendmsg 호출 수 = 발급된 zc id 수)
    pub sent: u64,
    /// 완료 통보로 확인된 전송 건수 (flush 제한 시간 안에 못 받으면 sent보다 작다)
    pub completed: u64,
    /// 그중 커널이 복사로 폴백한 건수
    pub copied: u64,
}

impl ZcStats {
    /// 실제 zero-copy 성공률(%). 100.0이 아니면 해당 실행은 무효 처리한다.
    pub fn success_pct(&self) -> f64 {
        if self.completed == 0 {
            return f64::NAN;
        }
        (self.completed - self.copied) as f64 * 100.0 / self.completed as f64
    }
}

/// cmsghdr 정렬(8바이트)을 보장하는 제어 메시지 버퍼.
/// `[u8; N]`은 정렬이 1이라 그 위의 `&cmsghdr` 참조가 정의되지 않은 동작이 될 수 있다.
#[repr(C, align(8))]
struct CmsgBuf([u8; 1024]);

/// errqueue를 비우고 완료된 zc id 범위 `(lo, hi)`를 `ranges`에 채운다 (기존 내용은 지움).
/// 완료·복사 폴백 건수는 `stats`에 누적한다.
///
/// 송신 경로에서 자주 불리므로 결과 버퍼는 호출자가 재사용한다 (매번 할당하지 않도록).
/// `fd`는 유효한 소켓이어야 한다.
pub fn drain_errqueue(fd: RawFd, stats: &mut ZcStats, ranges: &mut Vec<(u32, u32)>) {
    ranges.clear();
    let mut ctrl = CmsgBuf([0u8; 1024]);

    loop {
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_control = ctrl.0.as_mut_ptr() as *mut libc::c_void;
        msg.msg_controllen = ctrl.0.len() as _;

        let n = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_ERRQUEUE | libc::MSG_DONTWAIT) };
        if n < 0 {
            // EAGAIN == 더 이상 통보 없음
            break;
        }

        let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
        while !cmsg.is_null() {
            let c = unsafe { &*cmsg };
            let is_err_cmsg = (c.cmsg_level == libc::SOL_IP && c.cmsg_type == IP_RECVERR)
                || (c.cmsg_level == libc::SOL_IPV6 && c.cmsg_type == IPV6_RECVERR);

            if is_err_cmsg {
                let serr = unsafe { libc::CMSG_DATA(cmsg) as *const libc::sock_extended_err };
                let serr = unsafe { &*serr };

                if serr.ee_origin == SO_EE_ORIGIN_ZEROCOPY {
                    let lo = serr.ee_info;
                    let hi = serr.ee_data;
                    let copied = (serr.ee_code & SO_EE_CODE_ZEROCOPY_COPIED) != 0;

                    let count = hi.saturating_sub(lo).saturating_add(1) as u64;
                    stats.completed += count;
                    if copied {
                        stats.copied += count;
                    }
                    ranges.push((lo, hi));
                }
            }
            cmsg = unsafe { libc::CMSG_NXTHDR(&msg, cmsg) };
        }
    }
}

/// 고정 크기 버퍼 풀.
///
/// 각 `sendmsg(MSG_ZEROCOPY)` 호출은 소켓별 카운터를 1 증가시키며,
/// 완료 통보는 그 카운터 범위로 온다. 프레임 하나가 여러 번의 부분 전송으로
/// 나뉠 수 있으므로 버퍼당 미완료 id 개수를 세어 0이 될 때 회수한다.
///
/// 단, 청크 전송 중에는 "지금까지 등록된 청크가 모두 완료"되어 미완료 수가 잠깐 0이
/// 될 수 있다 (루프백에서는 sendmsg 반환 전에 완료 통보가 오기도 한다). 그때 회수하면
/// 아직 보낼 청크가 남은 버퍼가 재사용되고, 다음 청크 완료 때 free에 한 번 더 들어간다.
/// 그래서 `acquire()`부터 `finish_send()`까지는 "전송 중"으로 표시해 회수를 막고,
/// 버퍼는 acquire 1회당 정확히 한 번만 free로 돌아가게 한다.
pub struct BufPool {
    bufs: Vec<Vec<u8>>,
    free: VecDeque<usize>,
    /// buf idx -> 미완료 zc id 개수
    outstanding: Vec<u32>,
    /// buf idx -> acquire 후 send_frame이 아직 끝나지 않음
    in_send: Vec<bool>,
    /// zc id -> buf idx
    id_map: HashMap<u32, usize>,
    /// 다음에 발급될 zc id
    next_id: u32,
}

impl BufPool {
    pub fn new(count: usize, size: usize) -> Self {
        let bufs = (0..count).map(|_| vec![0u8; size]).collect();
        let free = (0..count).collect();
        Self {
            bufs,
            free,
            outstanding: vec![0; count],
            in_send: vec![false; count],
            id_map: HashMap::new(),
            next_id: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.bufs.len()
    }

    /// 재사용 가능한 버퍼 인덱스를 꺼낸다. 없으면 None.
    /// 꺼낸 버퍼는 `release_now()` 또는 `finish_send()` 전까지 회수되지 않는다.
    pub fn acquire(&mut self) -> Option<usize> {
        let idx = self.free.pop_front()?;
        self.in_send[idx] = true;
        Some(idx)
    }

    pub fn buf_mut(&mut self, idx: usize) -> &mut [u8] {
        &mut self.bufs[idx]
    }

    pub fn buf(&self, idx: usize) -> &[u8] {
        &self.bufs[idx]
    }

    /// 전송 1건(= sendmsg 1회)을 버퍼에 귀속시키고 zc id를 발급한다.
    pub fn register_send(&mut self, idx: usize) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.id_map.insert(id, idx);
        self.outstanding[idx] += 1;
        id
    }

    /// 복사 모드(zero-copy 미사용)에서는 전송 직후 즉시 회수한다.
    pub fn release_now(&mut self, idx: usize) {
        self.in_send[idx] = false;
        self.free.push_back(idx);
    }

    /// zero-copy 모드에서 프레임의 모든 청크를 넘긴 뒤 호출한다.
    /// 미완료 전송이 없으면 즉시, 있으면 마지막 완료 통보 때 회수된다.
    pub fn finish_send(&mut self, idx: usize) {
        self.in_send[idx] = false;
        if self.outstanding[idx] == 0 {
            self.free.push_back(idx);
        }
    }

    /// 완료 통보 범위를 반영해 버퍼를 회수한다 (전송 중인 버퍼는 `finish_send`가 회수).
    pub fn complete_range(&mut self, lo: u32, hi: u32) {
        let mut id = lo;
        loop {
            if let Some(idx) = self.id_map.remove(&id) {
                if self.outstanding[idx] > 0 {
                    self.outstanding[idx] -= 1;
                    if self.outstanding[idx] == 0 && !self.in_send[idx] {
                        self.free.push_back(idx);
                    }
                }
            }
            if id == hi {
                break;
            }
            id = id.wrapping_add(1);
        }
    }

    pub fn free_count(&self) -> usize {
        self.free.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 모든 버퍼를 꺼내 개수를 센다 — free에 같은 인덱스가 두 번 들어갔는지 확인용.
    fn drain_all(p: &mut BufPool) -> Vec<usize> {
        let mut v = Vec::new();
        while let Some(i) = p.acquire() {
            v.push(i);
        }
        v
    }

    #[test]
    fn buffer_is_not_reclaimed_mid_frame_even_if_registered_chunks_completed() {
        let mut p = BufPool::new(2, 16);
        let a = p.acquire().unwrap();

        // 청크 0 전송 → 프레임 전송이 끝나기 전에 완료 통보가 먼저 도착
        let id0 = p.register_send(a);
        p.complete_range(id0, id0);
        assert_eq!(p.free_count(), 1, "전송 중인 버퍼가 free로 돌아가면 안 된다");

        // 청크 1 전송 후 프레임 종료 — 청크 1은 아직 미완료
        let id1 = p.register_send(a);
        p.finish_send(a);
        assert_eq!(p.free_count(), 1);

        // 마지막 완료 통보에서 정확히 한 번 회수
        p.complete_range(id1, id1);
        assert_eq!(p.free_count(), 2);
        let mut all = drain_all(&mut p);
        all.sort_unstable();
        assert_eq!(all, vec![0, 1], "같은 버퍼가 free에 중복으로 들어가면 안 된다");
    }

    #[test]
    fn finish_send_reclaims_immediately_when_everything_completed() {
        let mut p = BufPool::new(1, 16);
        let a = p.acquire().unwrap();
        let id0 = p.register_send(a);
        let id1 = p.register_send(a);
        p.complete_range(id0, id1);
        assert_eq!(p.free_count(), 0);
        p.finish_send(a);
        assert_eq!(p.free_count(), 1);
        assert_eq!(drain_all(&mut p), vec![0]);
    }

    #[test]
    fn release_now_reclaims_in_copy_mode() {
        let mut p = BufPool::new(1, 16);
        let a = p.acquire().unwrap();
        assert_eq!(p.free_count(), 0);
        p.release_now(a);
        assert_eq!(drain_all(&mut p), vec![0]);
    }
}

/// 소켓에 SO_ZEROCOPY를 설정한다.
pub fn enable_zerocopy(fd: RawFd) -> std::io::Result<()> {
    let one: libc::c_int = 1;
    let r = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            SO_ZEROCOPY,
            &one as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if r < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
