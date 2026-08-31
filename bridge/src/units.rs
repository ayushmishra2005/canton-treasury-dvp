use anyhow::{anyhow, Result};

pub const DEMO_TOKEN_DECIMALS: u8 = 6;
pub const DEMO_WHOLE_TOKENS: u64 = 100_000;
pub const DEMO_BASE_UNITS: u64 = 100_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenUnits {
    pub decimals: u8,
    pub base_units: u64,
}

impl TokenUnits {
    pub fn from_base_units(base_units: u64, decimals: u8) -> Result<Self> {
        validate_decimals(decimals)?;
        if base_units == 0 {
            return Err(anyhow!("amount must be greater than zero"));
        }
        Ok(Self {
            decimals,
            base_units,
        })
    }

    pub fn from_whole_tokens(tokens: u64, decimals: u8) -> Result<Self> {
        let scale = scale_for(decimals)?;
        if tokens == 0 {
            return Err(anyhow!("token amount must be greater than zero"));
        }
        let base_units = tokens
            .checked_mul(scale)
            .ok_or_else(|| anyhow!("token amount overflows base units"))?;
        Ok(Self {
            decimals,
            base_units,
        })
    }

    pub fn from_canton_decimal(amount: &str, decimals: u8) -> Result<Self> {
        let base_units = canton_decimal_to_base_units(amount, decimals)?;
        Self::from_base_units(base_units, decimals)
    }

    pub fn canton_decimal(&self) -> Result<String> {
        base_units_to_canton_decimal(self.base_units, self.decimals)
    }

    pub fn whole_tokens(&self) -> Result<u64> {
        let scale = scale_for(self.decimals)?;
        if !self.base_units.is_multiple_of(scale) {
            return Err(anyhow!(
                "base units {} are not a whole number of tokens at {} decimals",
                self.base_units,
                self.decimals
            ));
        }
        Ok(self.base_units / scale)
    }
}

pub fn validate_decimals(decimals: u8) -> Result<()> {
    if decimals > 18 {
        return Err(anyhow!(
            "mint decimals {decimals} exceed the supported maximum of 18"
        ));
    }
    Ok(())
}

pub fn scale_for(decimals: u8) -> Result<u64> {
    validate_decimals(decimals)?;
    10u64
        .checked_pow(u32::from(decimals))
        .ok_or_else(|| anyhow!("decimal scale overflows u64"))
}

pub fn require_mint_decimals(actual: u8, expected: u8) -> Result<()> {
    validate_decimals(actual)?;
    if actual != expected {
        return Err(anyhow!(
            "configured mint decimals are {actual}, expected {expected}"
        ));
    }
    Ok(())
}

pub fn canton_decimal_to_base_units(amount: &str, decimals: u8) -> Result<u64> {
    validate_decimals(decimals)?;
    let amount = amount.trim();
    if amount.is_empty() {
        return Err(anyhow!("Canton amount is empty"));
    }
    if amount.starts_with('-') {
        return Err(anyhow!("Canton amount must not be negative"));
    }
    let (whole, frac) = match amount.split_once('.') {
        Some((whole, frac)) => (whole, frac),
        None => (amount, ""),
    };
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return Err(anyhow!("Canton amount whole part is invalid"));
    }
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return Err(anyhow!("Canton amount fractional part is invalid"));
    }
    if frac.len() > usize::from(decimals) {
        return Err(anyhow!(
            "Canton amount {amount} exceeds {decimals} decimal places"
        ));
    }
    let mut frac_owned = frac.to_string();
    while frac_owned.len() < usize::from(decimals) {
        frac_owned.push('0');
    }
    let whole_units: u64 = if whole.chars().all(|c| c == '0') {
        0
    } else {
        whole
            .parse::<u64>()
            .map_err(|_| anyhow!("Canton amount {amount} overflows u64 tokens"))?
    };
    let frac_units: u64 = if frac_owned.is_empty() {
        0
    } else {
        frac_owned
            .parse::<u64>()
            .map_err(|_| anyhow!("Canton amount {amount} overflows fractional units"))?
    };
    let scale = scale_for(decimals)?;
    let whole_base = whole_units
        .checked_mul(scale)
        .ok_or_else(|| anyhow!("Canton amount {amount} overflows base units"))?;
    let base_units = whole_base
        .checked_add(frac_units)
        .ok_or_else(|| anyhow!("Canton amount {amount} overflows base units"))?;
    if base_units == 0 {
        return Err(anyhow!("Canton amount must be greater than zero"));
    }
    Ok(base_units)
}

pub fn base_units_to_canton_decimal(base_units: u64, decimals: u8) -> Result<String> {
    if base_units == 0 {
        return Err(anyhow!("amount must be greater than zero"));
    }
    let scale = scale_for(decimals)?;
    let whole = base_units / scale;
    let frac = base_units % scale;
    Ok(format!(
        "{whole}.{:0>width$}",
        frac,
        width = usize::from(decimals)
    ))
}

pub fn demo_units() -> TokenUnits {
    TokenUnits {
        decimals: DEMO_TOKEN_DECIMALS,
        base_units: DEMO_BASE_UNITS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_base_unit_converts_exactly() {
        let units = TokenUnits::from_base_units(1, 6).unwrap();
        assert_eq!(units.canton_decimal().unwrap(), "0.000001");
        assert_eq!(
            TokenUnits::from_canton_decimal("0.000001", 6)
                .unwrap()
                .base_units,
            1
        );
        assert!(units.whole_tokens().is_err());
    }

    #[test]
    fn one_whole_token_converts_exactly() {
        let units = TokenUnits::from_whole_tokens(1, 6).unwrap();
        assert_eq!(units.base_units, 1_000_000);
        assert_eq!(units.canton_decimal().unwrap(), "1.000000");
        assert_eq!(
            TokenUnits::from_canton_decimal("1.000000", 6)
                .unwrap()
                .base_units,
            1_000_000
        );
        assert_eq!(
            TokenUnits::from_canton_decimal("1", 6).unwrap().base_units,
            1_000_000
        );
    }

    #[test]
    fn demo_one_hundred_thousand_tokens() {
        let units = TokenUnits::from_whole_tokens(DEMO_WHOLE_TOKENS, 6).unwrap();
        assert_eq!(units.base_units, DEMO_BASE_UNITS);
        assert_eq!(units.canton_decimal().unwrap(), "100000.000000");
        assert_eq!(demo_units(), units);
        assert_eq!(
            TokenUnits::from_canton_decimal("100000.000000", 6).unwrap(),
            units
        );
    }

    #[test]
    fn excess_precision_is_rejected() {
        assert!(canton_decimal_to_base_units("100000.0000001", 6).is_err());
        assert!(canton_decimal_to_base_units("1.0000001", 6).is_err());
        assert!(TokenUnits::from_canton_decimal("0.0000001", 6).is_err());
    }

    #[test]
    fn overflow_is_rejected() {
        assert!(TokenUnits::from_whole_tokens(u64::MAX, 6).is_err());
        assert!(scale_for(20).is_err());
        assert!(canton_decimal_to_base_units("18446744073709551616", 6).is_err());
        assert!(TokenUnits::from_base_units(0, 6).is_err());
    }

    #[test]
    fn zero_and_negative_are_rejected() {
        assert!(canton_decimal_to_base_units("0", 6).is_err());
        assert!(canton_decimal_to_base_units("0.000000", 6).is_err());
        assert!(canton_decimal_to_base_units("-1.000000", 6).is_err());
        assert!(canton_decimal_to_base_units("-0.000001", 6).is_err());
        assert!(TokenUnits::from_whole_tokens(0, 6).is_err());
    }

    #[test]
    fn exact_round_trip_conversion() {
        for base in [1_u64, 1_000_000, DEMO_BASE_UNITS, 42, 999_999] {
            let units = TokenUnits::from_base_units(base, 6).unwrap();
            let decimal = units.canton_decimal().unwrap();
            let back = TokenUnits::from_canton_decimal(&decimal, 6).unwrap();
            assert_eq!(back, units);
        }
    }

    #[test]
    fn mint_decimals_must_match_the_configured_mint() {
        require_mint_decimals(6, 6).unwrap();
        assert!(require_mint_decimals(9, 6).is_err());
    }
}
