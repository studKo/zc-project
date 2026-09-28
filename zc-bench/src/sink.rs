//! 전송 백엔드.
//!
//! Case G1/G2/G3는 **이 trait 구현만 교체**되고 나머지 코드는 완전히 동일하다.
//! 케이스별로 프로그램을 따로 만들면 "차이가 zero-copy 때문"이라고 말할 수 없다.
//! (G3 io_uring SEND_ZC 싱크는 아직 미구현)

use crate::util::last_os_error;
use crate::zcstat::{self, BufPool, ZcStats, MSG_ZEROCOPY};
use std::io::{self, Write};
use std::net::TcpStream;
use std::os::unix::io::{AsRawFd, RawFd};

pub trait FrameSink {
    /// 프레임 하나를 전송한다. 버퍼는 풀에서 꺼내 채운 뒤 넘긴다.
    ///
    /// `Ok`를 반환하면 풀에 빈 버퍼가 1개 이상 있어야 한다 (송신 루프가 다음 프레임에 바로 쓴다).
    fn send_frame(&mut self, pool: &mut BufPool, idx: usize, len: usize) -> io::Result<()>;

    /// 남은 완료 통보를 모두 수거한다 (측정 종료 시).
    fn flush(&mut self, _pool: &mut BufPool) -> io::Result<()> {
        Ok(())
    }

    fn stats(&self) -> ZcStats {
        ZcStats::default()
    }

    fn name(&self) -> &'static str;
}

// ──────────────────────── Case N: 전송 없음 ────────────────────────

/// 프레임 생성 비용만 분리 측정하기 위한 널 싱크.
#[derive(Default)]
pub struct NullSink;

impl NullSink {
    pub fn new() -> Self {
        Self
    }
}

impl FrameSink for NullSink {
    fn send_frame(&mut self, pool: &mut BufPool, idx: usize, len: usize) -> io::Result<()> {
        // 최적화로 제거되지 않도록 첫 바이트만 읽는다.
        std::hint::black_box((pool.buf(idx)[0], len));
        pool.release_now(idx);
        Ok(())
    }
    fn name(&self) -> &'static str {
        "null"
    }
}

// ─────────────────────── Case G1: 일반 전송 ───────────────────────

/// 평범한 `write()`. 커널이 유저 버퍼를 소켓 버퍼로 복사한다.
pub struct CopySink {
    stream: TcpStream,
    /// 1회 write 당 최대 바이트. 0이면 프레임 전체를 한 번에 보낸다.
    chunk: usize,
}

impl CopySink {
    pub fn new(stream: TcpStream, chunk: usize) -> Self {
        Self { stream, chunk }
    }
}

impl FrameSink for CopySink {
    fn send_frame(&mut self, pool: &mut BufPool, idx: usize, len: usize) -> io::Result<()> {
        // 복사 방식이므로 write 반환 즉시 버퍼 재사용 가능
        let unit = if self.chunk == 0 { len } else { self.chunk.min(len) };
        let res = (|| -> io::Result<()> {
            let mut off = 0usize;
            while off < len {
                let end = (off + unit).min(len);
                self.stream.write_all(&pool.buf(idx)[off..end])?;
                off = end;
            }
            Ok(())
        })();
        pool.release_now(idx);
        res
    }

    fn name(&self) -> &'static str {
        "copy"
    }
}

// ────────────────────── Case G2: MSG_ZEROCOPY ──────────────────────

/// ENOBUFS 연속 재시도 상한 (재시도마다 최대 1ms 대기 → 약 1초).
const ENOBUFS_MAX_RETRY: u32 = 1000;

/// 빈 버퍼를 기다리는 최대 시간. 링크가 끊기거나 수신 측이 멈추면 완료 통보가
/// 영영 오지 않으므로, 무한 대기 대신 에러로 끝낸다.
const BUFFER_WAIT_TIMEOUT_NS: u64 = 5_000_000_000;

/// `MSG_ZEROCOPY` 전송.
///
/// 커널이 유저 페이지를 직접 참조하므로, 완료 통보를 받기 전까지
/// 해당 버퍼를 재사용해서는 안 된다.
pub struct MsgZcSink {
    stream: TcpStream,
    fd: RawFd,
    stats: ZcStats,
    /// 1회 sendmsg 당 최대 바이트. 0이면 프레임 전체를 한 번에 보낸다.
    chunk: usize,
    /// drain_errqueue 결과 버퍼 (재사용해 송신 경로의 할당을 없앤다)
    ranges: Vec<(u32, u32)>,
}

impl MsgZcSink {
    pub fn new(stream: TcpStream, chunk: usize) -> io::Result<Self> {
        let fd = stream.as_raw_fd();
        zcstat::enable_zerocopy(fd)?;
        Ok(Self {
            stream,
            fd,
            stats: ZcStats::default(),
            chunk,
            ranges: Vec::with_capacity(64),
        })
    }

    /// errqueue를 비우고 완료된 버퍼를 풀에 되돌린다.
    fn reap(&mut self, pool: &mut BufPool) {
        zcstat::drain_errqueue(self.fd, &mut self.stats, &mut self.ranges);
        for &(lo, hi) in &self.ranges {
            pool.complete_range(lo, hi);
        }
    }

    /// errqueue에 완료 통보(POLLERR)가 뜰 때까지 최대 `timeout_ms` 대기한다.
    fn poll_errqueue(&self, timeout_ms: i32) {
        let mut pfd = libc::pollfd {
            fd: self.fd,
            events: 0,
            revents: 0,
        };
        unsafe {
            libc::poll(&mut pfd, 1, timeout_ms);
        }
    }

    /// 버퍼가 하나도 없으면 완료 통보를 기다린다.
    ///
    /// recvmsg를 반복 호출하는 스핀 대기는 zero-copy와 무관한 CPU를 G2 측정값에 더하므로,
    /// poll로 통보가 올 때까지 잠든 뒤 수거한다.
    fn wait_for_buffer(&mut self, pool: &mut BufPool) -> io::Result<()> {
        let deadline = crate::util::now_ns() + BUFFER_WAIT_TIMEOUT_NS;
        while pool.free_count() == 0 {
            if crate::util::now_ns() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "zero-copy 완료 통보가 5초간 오지 않아 빈 버퍼를 확보하지 못했습니다 (링크·수신 측 확인)",
                ));
            }
            self.poll_errqueue(10);
            self.reap(pool);
        }
        Ok(())
    }
}

impl FrameSink for MsgZcSink {
    fn send_frame(&mut self, pool: &mut BufPool, idx: usize, len: usize) -> io::Result<()> {
        let mut off = 0usize;
        let mut enobufs = 0u32;

        while off < len {
            let ptr = unsafe { pool.buf(idx).as_ptr().add(off) };
            let mut remain = len - off;
            if self.chunk != 0 {
                remain = remain.min(self.chunk);
            }

            let mut iov = libc::iovec {
                iov_base: ptr as *mut libc::c_void,
                iov_len: remain,
            };
            let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
            msg.msg_iov = &mut iov;
            msg.msg_iovlen = 1;

            let n = unsafe { libc::sendmsg(self.fd, &msg, MSG_ZEROCOPY) };
            if n < 0 {
                let e = last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted {
                    self.reap(pool);
                    continue;
                }
                // optmem 한도(net.core.optmem_max) 초과 — 완료 통보를 수거하면 풀리므로
                // 잠시 기다렸다 재시도한다. 이 호출은 zc id를 소비하지 않는다.
                if e.raw_os_error() == Some(libc::ENOBUFS) && enobufs < ENOBUFS_MAX_RETRY {
                    enobufs += 1;
                    self.reap(pool);
                    self.poll_errqueue(1);
                    continue;
                }
                return Err(e);
            }
            enobufs = 0;

            // 성공한 sendmsg 1회 = zc 카운터 1 증가
            pool.register_send(idx);
            self.stats.sent += 1;
            off += n as usize;
            // 청크마다 수거하지 않는다: recvmsg 시스템콜이 청크 수만큼 늘어 G2 CPU를 부풀린다.
            // 커널은 연속된 완료 통보를 범위 하나로 합쳐 두므로 프레임당 1회 수거로 충분하다.
        }

        // 프레임의 모든 청크를 넘겼다 — 이제부터 완료 통보로 회수될 수 있다
        // (그 전에는 등록된 청크가 전부 완료돼도 회수하지 않는다: BufPool 문서 참고)
        pool.finish_send(idx);

        // 프레임당 1회 수거 후, 다음 프레임을 위한 버퍼 확보
        self.reap(pool);
        self.wait_for_buffer(pool)
    }

    fn flush(&mut self, pool: &mut BufPool) -> io::Result<()> {
        // 소켓 종료 전, 남은 통보를 모두 수거
        let deadline = crate::util::now_ns() + 2_000_000_000;
        while pool.free_count() < pool.capacity() && crate::util::now_ns() < deadline {
            self.reap(pool);
            self.poll_errqueue(10);
        }
        let _ = self.stream.flush();
        Ok(())
    }

    fn stats(&self) -> ZcStats {
        self.stats.clone()
    }

    fn name(&self) -> &'static str {
        "msg_zc"
    }
}
