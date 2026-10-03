//! Parsing of Kubernetes resource quantities into byte counts.
//!
//! Supports the forms used for memory and storage: plain integers, decimal
//! fractions, binary suffixes (Ki, Mi, Gi, Ti, Pi, Ei), decimal suffixes
//! (k, M, G, T, P, E), and decimal exponents (1e9, 2E3). Values that do not
//! resolve to a whole number of bytes are rejected.

/// Errors parsing a quantity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuantityError(pub String);

impl std::fmt::Display for QuantityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid quantity: {}", self.0)
    }
}

impl std::error::Error for QuantityError {}

/// Parse a quantity into bytes.
pub fn to_bytes(q: &str) -> Result<u64, QuantityError> {
    let err = || QuantityError(q.to_owned());
    let s = q.trim();
    if s.is_empty() || s.starts_with('-') {
        return Err(err());
    }
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (number, suffix) = s.split_at(split);
    if number.is_empty() || number.matches('.').count() > 1 {
        return Err(err());
    }

    let (int_part, frac_part) = number.split_once('.').unwrap_or((number, ""));
    // Work in an exact rational: mantissa / 10^scale.
    let digits = format!("{int_part}{frac_part}");
    let mantissa: u128 = digits.parse().map_err(|_| err())?;
    let mut scale = u32::try_from(frac_part.len()).map_err(|_| err())?;

    let multiplier: u128 = match suffix {
        "" => 1,
        "Ki" => 1 << 10,
        "Mi" => 1 << 20,
        "Gi" => 1 << 30,
        "Ti" => 1 << 40,
        "Pi" => 1 << 50,
        "Ei" => 1 << 60,
        "k" => 10u128.pow(3),
        "M" => 10u128.pow(6),
        "G" => 10u128.pow(9),
        "T" => 10u128.pow(12),
        "P" => 10u128.pow(15),
        "E" => 10u128.pow(18),
        exp if exp.starts_with(['e', 'E']) => {
            let e: i32 = exp[1..].parse().map_err(|_| err())?;
            if e >= 0 {
                10u128
                    .checked_pow(u32::try_from(e).map_err(|_| err())?)
                    .ok_or_else(err)?
            } else {
                scale += e.unsigned_abs();
                1
            }
        }
        _ => return Err(err()),
    };

    let total = mantissa.checked_mul(multiplier).ok_or_else(err)?;
    let divisor = 10u128.checked_pow(scale).ok_or_else(err)?;
    if total % divisor != 0 {
        return Err(err());
    }
    u64::try_from(total / divisor).map_err(|_| err())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_common_forms() {
        assert_eq!(to_bytes("4Gi").unwrap(), 4 << 30);
        assert_eq!(to_bytes("512Mi").unwrap(), 512 << 20);
        assert_eq!(to_bytes("1.5Gi").unwrap(), 3 << 29);
        assert_eq!(to_bytes("2G").unwrap(), 2_000_000_000);
        assert_eq!(to_bytes("1500M").unwrap(), 1_500_000_000);
        assert_eq!(to_bytes("1e9").unwrap(), 1_000_000_000);
        assert_eq!(to_bytes("1024").unwrap(), 1024);
        assert_eq!(to_bytes(" 1Ki ").unwrap(), 1024);
    }

    #[test]
    fn rejects_invalid() {
        for q in ["", "-1Gi", "Gi", "1.2.3", "1Gib", "0.5", "1x", "100m"] {
            assert!(to_bytes(q).is_err(), "{q} should be rejected");
        }
    }
}
