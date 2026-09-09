//! QPP parameter validation (matching Go kcptun's `ValidateQPPParams`).
//!
//! Checks QPPCount and key length for safe QPP configuration.
//! Only available with the `qpp` feature.

/// Go qpp's `QPPMinimumSeedLength(8)` result.
const QPP_MIN_SEED_LENGTH: usize = 211;
/// Go qpp's `QPPMinimumPads(8)` result (`ceil(211 / 32)`).
const QPP_MIN_PADS: u16 = 7;
const QPP_POWER: u64 = 8;

/// Validate QPP parameters and return warnings for unsafe configurations.
///
/// Returns `Ok(warnings)` with a list of non-fatal warning messages, or
/// `Err(message)` for a fatal configuration error.
///
/// Checks performed (matching Go's `ValidateQPPParams`):
/// - QPPCount must be > 0 (fatal)
/// - Key must be at least `QPP_MIN_SEED_LENGTH` bytes (warning)
/// - QPPCount should meet minimum pad requirements (warning)
/// - QPPCount should be coprime with the QPP power, 8 (warning)
pub fn validate_qpp_params(count: u16, key: &[u8]) -> Result<Vec<String>, String> {
    if count == 0 {
        return Err("QPPCount must be greater than 0 when QPP is enabled".to_string());
    }

    let mut warnings = Vec::new();

    if key.len() < QPP_MIN_SEED_LENGTH {
        warnings.push(format!(
            "QPP Warning: 'key' has size of {} bytes, required {} bytes at least",
            key.len(),
            QPP_MIN_SEED_LENGTH
        ));
    }

    if count < QPP_MIN_PADS {
        warnings.push(format!(
            "QPP Warning: QPPCount {}, required {} at least",
            count, QPP_MIN_PADS
        ));
    }

    if gcd(count as u64, QPP_POWER) != 1 {
        warnings.push(format!(
            "QPP Warning: QPPCount {}, choose a prime number for security",
            count
        ));
    }

    Ok(warnings)
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_count_is_fatal() {
        let result = validate_qpp_params(0, &[0u8; QPP_MIN_SEED_LENGTH]);
        assert!(result.is_err());
    }

    #[test]
    fn short_key_warns() {
        let result = validate_qpp_params(61, b"short-key");
        assert!(result.is_ok());
        let warnings = result.unwrap();
        assert!(warnings.iter().any(|w| w.contains("key")));
    }

    #[test]
    fn adequate_key_no_warnings() {
        let result = validate_qpp_params(61, &[0u8; QPP_MIN_SEED_LENGTH]);
        assert!(result.is_ok());
        let warnings = result.unwrap();
        assert!(warnings.is_empty());
    }

    #[test]
    fn non_prime_warns() {
        let result = validate_qpp_params(64, &[0u8; QPP_MIN_SEED_LENGTH]);
        assert!(result.is_ok());
        let warnings = result.unwrap();
        assert!(warnings.iter().any(|w| w.contains("prime")));
    }

    #[test]
    fn too_few_pads_warns() {
        let result = validate_qpp_params(3, &[0u8; QPP_MIN_SEED_LENGTH]);
        assert!(result.is_ok());
        let warnings = result.unwrap();
        assert!(warnings.iter().any(|w| w.contains("required")));
    }
}
