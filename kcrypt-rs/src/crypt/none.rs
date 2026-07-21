//! No-op cipher. Passes data through unchanged.

use super::BlockCrypt;

#[derive(Debug)]
pub struct NoneCrypt;

impl BlockCrypt for NoneCrypt {
    fn encrypt(&self, _: &mut [u8]) {}
    fn decrypt(&self, _: &mut [u8]) {}
    fn name(&self) -> &'static str {
        "none"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn none0() {
        let c = NoneCrypt;
        let mut d = vec![1, 2, 3, 4];
        let o = d.clone();
        c.encrypt(&mut d);
        assert_eq!(d, o);
        c.decrypt(&mut d);
        assert_eq!(d, o);
    }
}
