//! 프레임 헤더 정의.
//!
//! L1(언어 레벨) zero-copy 실증 지점. `zerocopy` crate를 사용해
//! 바이트 슬라이스 위에 구조체를 **복사 없이** 캐스팅한다.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub const MAGIC: u32 = 0x5A43_4246; // "ZCBF"
pub const HEADER_SIZE: usize = 40;

/// `FrameHeader` 정렬(8바이트)을 보장하는 헤더 수신 버퍼.
///
/// `parse_ref`는 무복사 캐스팅이라 입력 슬라이스가 `FrameHeader`의 정렬을
/// 만족해야 한다. `[u8; N]` 스택 배열은 정렬이 1이어서 우연히 맞을 때만 동작하므로,
/// 헤더를 받아 `parse_ref`로 읽을 때는 반드시 이 타입을 쓴다.
#[repr(C, align(8))]
pub struct HeaderBuf(pub [u8; HEADER_SIZE]);

/// 전송 프레임 헤더 (40 bytes, repr(C) 고정 레이아웃).
///
/// `IntoBytes` : 구조체 -> 바이트 슬라이스 (복사 없음)
/// `FromBytes` : 바이트 슬라이스 -> 구조체 참조 (복사 없음)
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct FrameHeader {
    /// 매직 넘버. 프레임 경계 동기 확인용.
    pub magic: u32,
    /// 폭 (px)
    pub width: u16,
    /// 높이 (px)
    pub height: u16,
    /// 프레임 시퀀스 번호 (0부터 단조 증가)
    pub seq: u64,
    /// 송신 시각 (CLOCK_MONOTONIC_RAW, ns)
    pub tx_ns: u64,
    /// FourCC (NV12 등)
    pub fourcc: u32,
    /// 헤더를 제외한 페이로드 바이트 수
    pub payload: u32,
    /// 예약 (8바이트 정렬 유지)
    pub _pad: u64,
}

impl FrameHeader {
    pub fn new(seq: u64, tx_ns: u64, w: u16, h: u16, fourcc: u32, payload: u32) -> Self {
        Self {
            magic: MAGIC,
            width: w,
            height: h,
            seq,
            tx_ns,
            fourcc,
            payload,
            _pad: 0,
        }
    }

    /// 무복사 파싱. 슬라이스 위에 헤더를 그대로 겹쳐 읽는다.
    ///
    /// 반환된 참조는 입력 슬라이스의 생명주기에 묶인다 — 복사가 없으므로
    /// 원본 버퍼가 살아 있는 동안에만 유효하다는 제약이 타입으로 강제된다.
    ///
    /// 입력이 8바이트 정렬이 아니면 캐스팅이 거부되어 `None`이 된다.
    /// 수신 버퍼는 [`HeaderBuf`]를 쓴다.
    pub fn parse_ref(buf: &[u8]) -> Option<&FrameHeader> {
        let (h, _rest) = FrameHeader::ref_from_prefix(buf).ok()?;
        if h.magic == MAGIC {
            Some(h)
        } else {
            None
        }
    }

    /// 비교군: 복사해서 파싱. 정렬과 무관하게 동작한다.
    /// L1 보조 실험(`parse_ref` 대비 벤치마크, 미구현)의 대조군으로 남겨 둔다.
    #[allow(dead_code)]
    pub fn parse_copy(buf: &[u8]) -> Option<FrameHeader> {
        let (h, _rest) = FrameHeader::read_from_prefix(buf).ok()?;
        if h.magic == MAGIC {
            Some(h)
        } else {
            None
        }
    }

    /// 헤더를 대상 버퍼 앞부분에 기록 (복사 없이 바이트 뷰 사용)
    pub fn write_into(&self, buf: &mut [u8]) {
        buf[..HEADER_SIZE].copy_from_slice(self.as_bytes());
    }
}

/// NV12 FourCC
pub const FOURCC_NV12: u32 = u32::from_le_bytes(*b"NV12");

/// NV12 기준 프레임 페이로드 크기 (12 bpp)
pub const fn nv12_size(w: usize, h: usize) -> usize {
    w * h * 3 / 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_size_is_fixed() {
        assert_eq!(std::mem::size_of::<FrameHeader>(), HEADER_SIZE);
    }

    #[test]
    fn roundtrip_zero_copy() {
        let mut buf = vec![0u8; 128];
        let h = FrameHeader::new(42, 1234, 1280, 960, FOURCC_NV12, 1_843_200);
        h.write_into(&mut buf);

        let parsed = FrameHeader::parse_ref(&buf).expect("parse");
        assert_eq!(parsed.seq, 42);
        assert_eq!(parsed.payload, 1_843_200);
        assert_eq!(parsed.width, 1280);
    }

    #[test]
    fn nv12_1280x960() {
        assert_eq!(nv12_size(1280, 960), 1_843_200);
    }

    #[test]
    fn header_buf_satisfies_alignment() {
        assert!(std::mem::align_of::<HeaderBuf>() >= std::mem::align_of::<FrameHeader>());

        let mut hb = HeaderBuf([0u8; HEADER_SIZE]);
        FrameHeader::new(7, 1, 1280, 960, FOURCC_NV12, 1_843_200).write_into(&mut hb.0);
        let parsed = FrameHeader::parse_ref(&hb.0).expect("aligned parse");
        assert_eq!(parsed.seq, 7);
    }

    #[test]
    fn misaligned_input_is_rejected_by_parse_ref() {
        // 정렬이 보장된 버퍼에서 1바이트 어긋난 위치에 헤더를 쓴다 → 확실히 오정렬
        #[repr(C, align(8))]
        struct Wide([u8; HEADER_SIZE + 8]);
        let mut w = Wide([0u8; HEADER_SIZE + 8]);
        FrameHeader::new(9, 2, 1280, 960, FOURCC_NV12, 1_843_200).write_into(&mut w.0[1..]);

        // 무복사 캐스팅은 정렬 불일치로 거부되고
        assert!(FrameHeader::parse_ref(&w.0[1..]).is_none());
        // 복사 파싱은 정렬과 무관하게 성공한다
        assert_eq!(FrameHeader::parse_copy(&w.0[1..]).expect("copy parse").seq, 9);
    }
}
