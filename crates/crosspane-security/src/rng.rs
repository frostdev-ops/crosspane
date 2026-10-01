//! Randomness, injectable so state machines are testable with fixed inputs.

/// A source of cryptographically secure random bytes.
pub trait Rng: Send {
    fn fill(&mut self, buf: &mut [u8]);
}

/// The system CSPRNG through aws-lc-rs.
#[derive(Debug, Default)]
pub struct SystemRng;

impl Rng for SystemRng {
    fn fill(&mut self, buf: &mut [u8]) {
        use aws_lc_rs::rand::SecureRandom;
        // The system RNG fails only if the OS can't provide randomness at all; nothing secure can
        // continue then.
        #[allow(clippy::expect_used)]
        aws_lc_rs::rand::SystemRandom::new()
            .fill(buf)
            .expect("the operating system's random number generator failed");
    }
}
