//! zc-bench — Linux/Rust zero-copy raw 비디오 전송 벤치마크
//!
//! 사용 예:
//!   수신: zc-bench --mode receiver --port 5001
//!   송신: zc-bench --mode sender --sink msg_zc --target 127.0.0.1:5001 --duration 30

mod frame;
mod sink;
mod stats;
mod util;
mod zcstat;

use frame::{nv12_size, FrameHeader, HeaderBuf, FOURCC_NV12, HEADER_SIZE};
use sink::{CopySink, FrameSink, MsgZcSink, NullSink};
use stats::{FrameRecord, Run, RunConfig};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::sync::Arc;
use std::time::Duration;
use zcstat::BufPool;

const ECHO_REC_SIZE: usize = 16; // seq(8) + rx_ns(8)

/// 송신 측 연결 타임아웃 — 수신 측이 꺼져 있을 때 SYN 재전송(수십 초~2분) 동안 멈추지 않도록
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// 수신 측이 헤더의 payload 값만 믿고 할당하지 않도록 두는 상한 (1280x960 NV12는 약 1.8 MB)
const MAX_PAYLOAD: usize = 64 << 20;

struct Args {
    mode: String,
    sink: String,
    target: String,
    port: u16,
    width: u32,
    height: u32,
    fps: u32,
    duration: u64,
    warmup: u64,
    pool: usize,
    echo: bool,
    out: Option<String>,
    raw: Option<String>,
    case: String,
    tag: String,
    pass: String,
    mtu: u32,
    chunk: usize,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            mode: "sender".into(),
            sink: "copy".into(),
            target: "127.0.0.1:5001".into(),
            port: 5001,
            width: 1280,
            height: 960,
            fps: 30,
            duration: 30,
            warmup: 5,
            pool: 8,
            echo: false,
            out: None,
            raw: None,
            case: "G1".into(),
            tag: String::new(),
            pass: "cpu".into(),
            mtu: 0,
            chunk: 0,
        }
    }
}

fn usage() -> ! {
    eprintln!(
        r#"zc-bench — zero-copy raw video transfer benchmark

공통
  --mode      sender | receiver        (기본 sender)

송신 옵션
  --sink      null | copy | msg_zc     (기본 copy)
                null   : 전송 없음 (프레임 생성 비용 분리)
                copy   : 일반 write()          [Case G1]
                msg_zc : MSG_ZEROCOPY          [Case G2]
  --target    IP:PORT                  (기본 127.0.0.1:5001, 실측은 192.168.50.2:5001)
  --width     px                       (기본 1280)
  --height    px                       (기본 960)
  --fps       N                        (기본 30)
  --duration  초                       (기본 30, 워밍업 포함 전체 송신 시간)
  --warmup    초                       (기본 5, 이 구간 데이터·CPU는 제외)
  --pool      버퍼 개수                (기본 8)
  --echo                               지연 측정용 echo 채널 사용 (echo RTT/2 → lat_* 컬럼)
  --case      라벨                     (기본 G1)
  --tag       실행 식별자              (복사시간 데이터 병합 키, 기본 case와 동일)
  --pass      cpu | copy               (기본 cpu. copy = bpftrace 동시 실행 패스 — 결과 분리용 라벨)
  --mtu       N                        (기록용 MTU 라벨, 기본 0=미지정. run_matrix.sh가 자동 지정)
  --chunk     전송 단위 바이트         (0=프레임 전체, 기본 0)
  --out       요약 CSV 경로 (append, 헤더가 다른 기존 파일에는 쓰지 않음)
  --raw       프레임 단위 CSV 경로

수신 옵션
  --port      수신 포트                (기본 5001, echo는 port+1)

예시
  # 터미널 1
  zc-bench --mode receiver --port 5001
  # 터미널 2
  zc-bench --mode sender --sink msg_zc --target 127.0.0.1:5001 \
           --echo --duration 30 --case G2 --out data/summary.csv
"#
    );
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut a = Args::default();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let k = argv[i].as_str();
        let next = |i: &mut usize| -> String {
            *i += 1;
            argv.get(*i).cloned().unwrap_or_else(|| usage())
        };
        match k {
            "--mode" => a.mode = next(&mut i),
            "--sink" => a.sink = next(&mut i),
            "--target" => a.target = next(&mut i),
            "--port" => a.port = next(&mut i).parse().unwrap_or_else(|_| usage()),
            "--width" => a.width = next(&mut i).parse().unwrap_or_else(|_| usage()),
            "--height" => a.height = next(&mut i).parse().unwrap_or_else(|_| usage()),
            "--fps" => a.fps = next(&mut i).parse().unwrap_or_else(|_| usage()),
            "--duration" => a.duration = next(&mut i).parse().unwrap_or_else(|_| usage()),
            "--warmup" => a.warmup = next(&mut i).parse().unwrap_or_else(|_| usage()),
            "--pool" => a.pool = next(&mut i).parse().unwrap_or_else(|_| usage()),
            "--case" => a.case = next(&mut i),
            "--tag" => a.tag = next(&mut i),
            "--pass" => a.pass = next(&mut i),
            "--mtu" => a.mtu = next(&mut i).parse().unwrap_or_else(|_| usage()),
            "--chunk" => a.chunk = next(&mut i).parse().unwrap_or_else(|_| usage()),
            "--out" => a.out = Some(next(&mut i)),
            "--raw" => a.raw = Some(next(&mut i)),
            "--echo" => a.echo = true,
            "-h" | "--help" => usage(),
            _ => {
                eprintln!("unknown option: {}", k);
                usage()
            }
        }
        i += 1;
    }
    if a.pass != "cpu" && a.pass != "copy" {
        eprintln!("--pass 는 cpu 또는 copy 여야 합니다: {}", a.pass);
        usage()
    }
    if a.mode == "sender" && a.warmup >= a.duration {
        eprintln!(
            "--warmup({}s)은 --duration({}s)보다 짧아야 합니다 (측정 구간이 0이 됨)",
            a.warmup, a.duration
        );
        usage()
    }
    a
}

fn main() {
    let args = parse_args();
    let r = match args.mode.as_str() {
        "sender" => run_sender(&args),
        "receiver" => run_receiver(&args),
        _ => usage(),
    };
    if let Err(e) = r {
        eprintln!("error: {}", e);
        std::process::exit(1);
    }
}

// ───────────────────────────── SENDER ─────────────────────────────

fn run_sender(args: &Args) -> std::io::Result<()> {
    // run_ns(총 실행 시간)의 기준점 — 버퍼 준비·소켓 연결 전
    let proc_start_ns = util::now_ns();

    let payload = nv12_size(args.width as usize, args.height as usize);
    let frame_len = HEADER_SIZE + payload;

    println!(
        "sender: {}x{} NV12, frame={} B ({:.2} MB), {} fps → {:.1} Mbps",
        args.width,
        args.height,
        frame_len,
        frame_len as f64 / 1e6,
        args.fps,
        frame_len as f64 * 8.0 * args.fps as f64 / 1e6
    );

    let mut pool = BufPool::new(args.pool, frame_len);

    // 합성 패턴을 버퍼에 미리 채워둔다 — 매 프레임 생성 비용이 측정을 오염시키지 않도록
    for i in 0..pool.capacity() {
        let b = pool.buf_mut(i);
        for (j, x) in b[HEADER_SIZE..].iter_mut().enumerate() {
            *x = (j % 251) as u8;
        }
    }

    // 싱크 준비
    let mut sink: Box<dyn FrameSink> = match args.sink.as_str() {
        "null" => Box::new(NullSink::new()),
        "copy" => Box::new(CopySink::new(connect(&args.target)?, args.chunk)),
        "msg_zc" => Box::new(MsgZcSink::new(connect(&args.target)?, args.chunk)?),
        other => {
            eprintln!("unknown sink: {}", other);
            usage()
        }
    };

    // echo 채널 (지연 측정)
    let echo_on = args.echo && args.sink != "null";
    let (echo_tx, echo_rx) = mpsc::channel::<(u64, u64)>();
    let stop = Arc::new(AtomicBool::new(false));
    if echo_on {
        let mut es = connect(&echo_addr(&args.target)?)?;
        let stop2 = stop.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; ECHO_REC_SIZE];
            while !stop2.load(Ordering::Relaxed) {
                match es.read_exact(&mut buf) {
                    Ok(()) => {
                        let seq = u64::from_le_bytes(buf[0..8].try_into().unwrap());
                        let _rx_ns = u64::from_le_bytes(buf[8..16].try_into().unwrap());
                        let _ = echo_tx.send((seq, util::now_ns()));
                    }
                    Err(_) => break,
                }
            }
        });
    }

    let tag = if args.tag.is_empty() { args.case.clone() } else { args.tag.clone() };
    let mut run = Run::new(&RunConfig {
        case: &args.case,
        tag: &tag,
        pass: &args.pass,
        sink: sink.name(),
        width: args.width,
        height: args.height,
        fps: args.fps,
        mtu: args.mtu,
        chunk_bytes: args.chunk,
        warmup_s: args.warmup,
        proc_start_ns,
    });

    let interval_ns = 1_000_000_000u64 / args.fps.max(1) as u64;
    let end_ns = util::now_ns() + args.duration * 1_000_000_000;
    let mut deadline = util::now_ns();
    let mut seq: u64 = 0;
    let mut echo_seen: u64 = 0;

    while util::now_ns() < end_ns {
        deadline += interval_ns;
        // 워밍업이 끝나는 시점에 CPU 기준점을 찍는다
        run.tick(util::now_ns());

        // 싱크는 send_frame이 Ok면 빈 버퍼를 1개 이상 남긴다 (MsgZcSink는 완료 통보를 기다려 확보).
        // 버퍼 대기로 늦어진 프레임은 아래 late로 기록된다.
        let idx = pool.acquire().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "버퍼 풀 고갈 — 싱크가 빈 버퍼를 확보하지 못했습니다")
        })?;

        let tx_ns = util::now_ns();
        let h = FrameHeader::new(
            seq,
            tx_ns,
            args.width as u16,
            args.height as u16,
            FOURCC_NV12,
            payload as u32,
        );
        h.write_into(pool.buf_mut(idx));

        sink.send_frame(&mut pool, idx, frame_len)?;

        // 한 주기 안에 끝나지 못했으면 late로 표시하고 기준 시각을 현재로 당긴다.
        // 재동기화하지 않으면 뒤처진 만큼 이후 프레임을 쉬지 않고 몰아 보내는 버스트가 생긴다.
        let now = util::now_ns();
        let late = now > deadline;
        if late {
            deadline = now;
        }

        run.push(FrameRecord {
            seq,
            tx_ns,
            echo_ns: 0,
            latency_ns: 0,
            bytes: frame_len as u64,
            late,
        });
        seq += 1;

        // echo 수거 (논블로킹)
        while let Ok((eseq, when)) = echo_rx.try_recv() {
            apply_echo(&mut run, eseq, when);
            echo_seen += 1;
        }

        util::sleep_until(deadline);
    }

    // 측정 구간은 여기까지 (flush·잔여 echo 대기는 %CPU에 넣지 않는다)
    run.end_measure();
    sink.flush(&mut pool)?;
    run.finish();

    // 잔여 echo 수거 — 아직 도착하지 않은 echo가 있으면 최대 500ms 기다린다.
    // stop은 이 루프가 끝난 뒤에 세운다: 먼저 세우면 echo 스레드가 한 건만 더 넘기고
    // 종료해 채널이 끊기므로(Disconnected) 대기가 사실상 무효가 된다.
    if echo_on {
        let grace = util::now_ns() + 500_000_000;
        while echo_seen < seq && util::now_ns() < grace {
            match echo_rx.try_recv() {
                Ok((eseq, when)) => {
                    apply_echo(&mut run, eseq, when);
                    echo_seen += 1;
                }
                Err(TryRecvError::Empty) => std::thread::sleep(std::time::Duration::from_millis(2)),
                Err(TryRecvError::Disconnected) => break,
            }
        }
    }
    stop.store(true, Ordering::Relaxed);

    let zc = sink.stats();
    let summary = run.summary(&zc);
    summary.print();

    // 측정 구간에 프레임이 없으면 0으로 채워진 행이 유효한 결과처럼 섞이므로 기록하지 않는다
    if summary.frames == 0 || summary.elapsed_s < 1e-6 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Other,
            "측정 구간(워밍업 이후)에 전송된 프레임이 없습니다 — summary/raw CSV를 기록하지 않습니다",
        ));
    }

    if let Some(p) = &args.out {
        if let Some(dir) = std::path::Path::new(p).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        summary.append_csv(p)?;
        println!("summary  → {}", p);
    }
    if let Some(p) = &args.raw {
        if let Some(dir) = std::path::Path::new(p).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        run.write_raw_csv(p)?;
        println!("raw csv  → {}", p);
    }

    if zc.completed > 0 && zc.success_pct() < 99.99 {
        eprintln!(
            "\n[경고] zero-copy 성공률 {:.2}% — 이 실행은 성능 비교에 사용할 수 없습니다.",
            zc.success_pct()
        );
        eprintln!("  루프백(127.0.0.1)이면 정상입니다. 실제 NIC라면 다음을 확인하세요:");
        eprintln!("    ethtool -k <iface> | grep scatter-gather");
    }
    Ok(())
}

/// echo 도착 시각을 해당 프레임 기록에 반영한다 (지연 = RTT/2).
/// 송신 루프는 프레임마다 기록을 정확히 하나씩 추가하므로 `records[seq]`가 곧 그 프레임이다.
fn apply_echo(run: &mut Run, seq: u64, when: u64) {
    if let Some(rec) = run.records.get_mut(seq as usize) {
        rec.echo_ns = when;
        rec.latency_ns = when.saturating_sub(rec.tx_ns) / 2;
    }
}

fn invalid_input(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// `IP:PORT`에서 echo 채널 주소(`IP:PORT+1`)를 만든다.
fn echo_addr(target: &str) -> io::Result<String> {
    let (host, p) = target
        .rsplit_once(':')
        .ok_or_else(|| invalid_input(format!("--target은 IP:PORT 형식이어야 합니다: {}", target)))?;
    let port: u16 = p
        .parse()
        .map_err(|_| invalid_input(format!("--target의 포트가 올바르지 않습니다: {}", target)))?;
    let echo_port = port
        .checked_add(1)
        .ok_or_else(|| invalid_input("포트 65535는 echo 포트(+1)를 만들 수 없습니다".into()))?;
    Ok(format!("{}:{}", host, echo_port))
}

/// 타임아웃을 두고 TCP 연결한다 (주소가 여러 개로 해석되면 차례로 시도).
fn connect(target: &str) -> io::Result<TcpStream> {
    let mut last_err = None;
    for addr in target.to_socket_addrs()? {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(s) => {
                s.set_nodelay(true)?;
                return Ok(s);
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| invalid_input(format!("주소를 해석할 수 없습니다: {}", target))))
}

// ──────────────────────────── RECEIVER ────────────────────────────

fn run_receiver(args: &Args) -> std::io::Result<()> {
    let echo_port = args
        .port
        .checked_add(1)
        .ok_or_else(|| invalid_input("포트 65535는 echo 포트(+1)를 만들 수 없습니다".into()))?;
    let data_l = TcpListener::bind(("0.0.0.0", args.port))?;
    let echo_l = TcpListener::bind(("0.0.0.0", echo_port))?;
    println!("receiver: data :{}  echo :{}  (대기 중)", args.port, echo_port);

    loop {
        let (mut data, peer) = data_l.accept()?;
        data.set_nodelay(true)?;
        println!("connected: {}", peer);

        // 송신 측이 --echo를 쓰면 곧바로 echo 연결이 들어온다.
        echo_l.set_nonblocking(true)?;
        let mut echo: Option<TcpStream> = None;
        let deadline = util::now_ns() + 500_000_000;
        while util::now_ns() < deadline {
            match echo_l.accept() {
                Ok((s, _)) => {
                    s.set_nodelay(true)?;
                    echo = Some(s);
                    break;
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
        echo_l.set_nonblocking(false)?;
        println!("echo: {}", if echo.is_some() { "on" } else { "off" });

        serve(&mut data, echo.as_mut())?;
        println!("peer closed\n");
    }
}

fn serve(data: &mut TcpStream, mut echo: Option<&mut TcpStream>) -> std::io::Result<()> {
    // 정렬이 보장된 버퍼 — parse_ref(무복사 캐스팅)는 8바이트 정렬을 요구한다
    let mut head = HeaderBuf([0u8; HEADER_SIZE]);
    let mut sink_buf = vec![0u8; 4 << 20];
    let mut frames: u64 = 0;
    let mut bytes: u64 = 0;
    let t0 = util::now_ns();
    let cpu0 = util::self_cpu_ns();

    loop {
        if data.read_exact(&mut head.0).is_err() {
            break;
        }
        // 무복사 파싱 — 수신 버퍼 위에 헤더를 그대로 겹쳐 읽는다
        let (seq, payload) = match FrameHeader::parse_ref(&head.0) {
            Some(h) => (h.seq, h.payload as usize),
            None => {
                eprintln!("frame desync — magic mismatch");
                break;
            }
        };
        if payload > MAX_PAYLOAD {
            eprintln!("frame desync — payload {} B가 상한 {} B를 넘습니다", payload, MAX_PAYLOAD);
            break;
        }

        if sink_buf.len() < payload {
            sink_buf.resize(payload, 0);
        }
        if data.read_exact(&mut sink_buf[..payload]).is_err() {
            break;
        }

        let rx_ns = util::now_ns();
        frames += 1;
        bytes += (HEADER_SIZE + payload) as u64;

        if let Some(e) = echo.as_mut() {
            let mut rec = [0u8; ECHO_REC_SIZE];
            rec[0..8].copy_from_slice(&seq.to_le_bytes());
            rec[8..16].copy_from_slice(&rx_ns.to_le_bytes());
            if e.write_all(&rec).is_err() {
                echo = None;
            }
        }
    }

    let elapsed = (util::now_ns() - t0) as f64 / 1e9;
    let cpu = (util::self_cpu_ns() - cpu0) as f64 / 1e9;
    println!(
        "  received {} frames, {:.1} MB in {:.1}s → {:.1} Mbps | rx CPU {:.2}%",
        frames,
        bytes as f64 / 1e6,
        elapsed,
        bytes as f64 * 8.0 / elapsed.max(1e-9) / 1e6,
        cpu / elapsed.max(1e-9) * 100.0
    );
    Ok(())
}
