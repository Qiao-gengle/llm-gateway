//! 时间源与随机源（见 design.md“时间源与随机源”）。

use chrono::{DateTime, Utc};

pub trait Clock: Send + Sync + 'static {
    /// 单调时钟，用于超时、熔断冷却、缓存过期、延迟统计
    fn now_instant(&self) -> std::time::Instant;
    /// 墙钟，用于 expires_at 比较、限流窗口、created_at
    fn now_utc(&self) -> DateTime<Utc>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_instant(&self) -> std::time::Instant {
        std::time::Instant::now()
    }
    fn now_utc(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// 可手动推进的确定性时钟；两个读数同步前进。
#[cfg(any(test, feature = "testkit"))]
#[derive(Debug)]
pub struct MockClock {
    state: std::sync::Mutex<(std::time::Instant, DateTime<Utc>)>,
}

#[cfg(any(test, feature = "testkit"))]
impl MockClock {
    pub fn new() -> Self {
        Self::starting_at(Utc::now())
    }

    pub fn starting_at(utc: DateTime<Utc>) -> Self {
        Self {
            state: std::sync::Mutex::new((std::time::Instant::now(), utc)),
        }
    }

    pub fn advance(&self, d: std::time::Duration) {
        let mut s = self.state.lock().unwrap();
        s.0 += d;
        s.1 += chrono::Duration::from_std(d).expect("duration out of range");
    }
}

#[cfg(any(test, feature = "testkit"))]
impl Default for MockClock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(any(test, feature = "testkit"))]
impl Clock for MockClock {
    fn now_instant(&self) -> std::time::Instant {
        self.state.lock().unwrap().0
    }
    fn now_utc(&self) -> DateTime<Utc> {
        self.state.lock().unwrap().1
    }
}

pub trait RandomSource: Send + Sync + 'static {
    /// 返回 [0, upper) 内均匀分布的整数，upper ≥ 1
    fn gen_range_u64(&self, upper: u64) -> u64;
    fn fill_bytes(&self, buf: &mut [u8]);
}

/// 操作系统随机源。Client_Key 与 AES-GCM nonce 必须使用它。
#[derive(Debug, Default, Clone, Copy)]
pub struct OsRandom;

impl RandomSource for OsRandom {
    fn gen_range_u64(&self, upper: u64) -> u64 {
        use rand::Rng;
        assert!(upper >= 1, "upper must be >= 1");
        rand::rngs::OsRng.gen_range(0..upper)
    }
    fn fill_bytes(&self, buf: &mut [u8]) {
        use rand::RngCore;
        rand::rngs::OsRng.fill_bytes(buf);
    }
}

/// 由种子确定的随机源，仅用于测试。
#[cfg(any(test, feature = "testkit"))]
#[derive(Debug)]
pub struct SeededRandom(std::sync::Mutex<rand_chacha::ChaCha8Rng>);

#[cfg(any(test, feature = "testkit"))]
impl SeededRandom {
    pub fn new(seed: u64) -> Self {
        use rand::SeedableRng;
        Self(std::sync::Mutex::new(rand_chacha::ChaCha8Rng::seed_from_u64(seed)))
    }
}

#[cfg(any(test, feature = "testkit"))]
impl RandomSource for SeededRandom {
    fn gen_range_u64(&self, upper: u64) -> u64 {
        use rand::Rng;
        assert!(upper >= 1, "upper must be >= 1");
        self.0.lock().unwrap().gen_range(0..upper)
    }
    fn fill_bytes(&self, buf: &mut [u8]) {
        use rand::RngCore;
        self.0.lock().unwrap().fill_bytes(buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn mock_clock_advances_both_readings() {
        let c = MockClock::new();
        let (i0, u0) = (c.now_instant(), c.now_utc());
        c.advance(Duration::from_secs(5));
        assert_eq!(c.now_instant() - i0, Duration::from_secs(5));
        assert_eq!(c.now_utc() - u0, chrono::Duration::seconds(5));
    }

    #[test]
    fn seeded_random_is_deterministic_and_in_range() {
        let (a, b) = (SeededRandom::new(7), SeededRandom::new(7));
        for _ in 0..100 {
            let x = a.gen_range_u64(10);
            assert!(x < 10);
            assert_eq!(x, b.gen_range_u64(10));
        }
        assert_eq!(SeededRandom::new(1).gen_range_u64(1), 0);
        let (mut p, mut q) = ([0u8; 16], [0u8; 16]);
        a.fill_bytes(&mut p);
        b.fill_bytes(&mut q);
        assert_eq!(p, q);
    }

    #[test]
    fn os_random_range_and_fill() {
        assert!(OsRandom.gen_range_u64(3) < 3);
        let mut buf = [0u8; 32];
        OsRandom.fill_bytes(&mut buf);
        assert_ne!(buf, [0u8; 32]);
    }

    #[test]
    fn system_clock_is_monotonic() {
        let c = SystemClock;
        let a = c.now_instant();
        assert!(c.now_instant() >= a);
    }
}
