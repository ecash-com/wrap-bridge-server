/// Converts an ECX deposit (`amount_in_sat`, ECX's 8-decimal base unit) into the wECX amount to
/// pay out, in wECX's own base units (`wecx_decimals`, fetched live from the mint - see
/// `solana::SolanaWallet::decimals`). The bridge is a flat 1:1 peg, not an AMM curve like the old
/// BTC/ECX swap: wECX is paid out from a treasury balance rather than priced against one, so there
/// is no pool to feed and no slippage - `fee_bps` is the only thing that isn't a pure unit
/// conversion (see `config::pegin_fee_bps`; this same function's signature works for the reverse
/// peg-out direction too, once that exists, given wECX-in/ECX-out amounts and
/// `config::pegout_fee_bps`).
///
/// Computed as a single `u128` fraction (`amount_in_sat * (10_000 - fee_bps) * 10^wecx_decimals /
/// (10_000 * 10^8)`) rather than applying the fee and the decimal rescale as two separate integer
/// divisions, so there's only one floor/rounding step instead of two compounding ones.
pub fn ecx_sat_to_wecx_base_units(amount_in_sat: u64, wecx_decimals: u8, fee_bps: u64) -> u64 {
    const ECX_DECIMALS: u32 = 8;
    let numerator = amount_in_sat as u128 * (10_000u128.saturating_sub(fee_bps as u128));
    let (numerator, denominator) = if wecx_decimals as u32 >= ECX_DECIMALS {
        (
            numerator * 10u128.pow(wecx_decimals as u32 - ECX_DECIMALS),
            10_000u128,
        )
    } else {
        (
            numerator,
            10_000u128 * 10u128.pow(ECX_DECIMALS - wecx_decimals as u32),
        )
    };
    // Saturate rather than wrap: a wrapped value would look like a small, plausible payout for what
    // is really an astronomically large deposit. Callers bound `amount_in_sat` (see
    // `config::max_deposit_amount`), so this only matters as a backstop.
    u64::try_from(numerator / denominator).unwrap_or(u64::MAX)
}

/// Peg-in payout after a fixed ECX deduction and then the percentage fee. The flat amount is the
/// cost of later spending the deposit UTXO, including a dust output the sender may have attached,
/// so it is removed before the percentage rather than also being charged 1%.
pub fn pegin_payout_base_units(
    amount_in_sat: u64,
    wecx_decimals: u8,
    fee_bps: u64,
    flat_fee_sat: u64,
) -> u64 {
    ecx_sat_to_wecx_base_units(
        amount_in_sat.saturating_sub(flat_fee_sat),
        wecx_decimals,
        fee_bps,
    )
}

/// Smallest deposit that meets `minimum_sat` and still pays out after the flat fee and percentage.
/// A configured minimum below the flat fee cannot be bridged, so the effective floor rises.
pub fn minimum_pegin_deposit_sat(
    minimum_sat: u64,
    wecx_decimals: u8,
    fee_bps: u64,
    flat_fee_sat: u64,
) -> u64 {
    if fee_bps >= 10_000 {
        return u64::MAX;
    }
    const ECX_DECIMALS: u32 = 8;
    let keep = 10_000u128 - fee_bps as u128;
    let (scale_up, scale_down) = if wecx_decimals as u32 >= ECX_DECIMALS {
        (10u128.pow(wecx_decimals as u32 - ECX_DECIMALS), 1u128)
    } else {
        (1u128, 10u128.pow(ECX_DECIMALS - wecx_decimals as u32))
    };
    let scaled_keep = keep * scale_up;
    if scaled_keep == 0 {
        return u64::MAX;
    }
    let min_net = (10_000u128 * scale_down + scaled_keep - 1) / scaled_keep;
    let min_net = u64::try_from(min_net).unwrap_or(u64::MAX);
    minimum_sat.max(flat_fee_sat.saturating_add(min_net))
}

/// Converts a wECX redemption (`amount_in_base_units`, wECX's own base units) into the ECX amount
/// to pay out, in ECX's 8-decimal base unit (`sat`). The peg-out mirror of
/// `ecx_sat_to_wecx_base_units` - same flat-peg-minus-fee shape with the two decimal scales
/// swapped, using `config::pegout_fee_bps` instead of `pegin_fee_bps`.
pub fn wecx_base_units_to_ecx_sat(amount_in_base_units: u64, wecx_decimals: u8, fee_bps: u64) -> u64 {
    const ECX_DECIMALS: u32 = 8;
    let numerator = amount_in_base_units as u128 * (10_000u128.saturating_sub(fee_bps as u128));
    let (numerator, denominator) = if ECX_DECIMALS >= wecx_decimals as u32 {
        (
            numerator * 10u128.pow(ECX_DECIMALS - wecx_decimals as u32),
            10_000u128,
        )
    } else {
        (
            numerator,
            10_000u128 * 10u128.pow(wecx_decimals as u32 - ECX_DECIMALS),
        )
    };
    u64::try_from(numerator / denominator).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pegout_same_decimals_no_fee_is_a_straight_passthrough() {
        assert_eq!(wecx_base_units_to_ecx_sat(1_000_000, 8, 0), 1_000_000);
    }

    #[test]
    fn pegout_same_decimals_with_fee_applies_flat_bps() {
        assert_eq!(wecx_base_units_to_ecx_sat(1_000_000, 8, 500), 950_000);
    }

    #[test]
    fn pegout_fewer_wecx_decimals_scales_up() {
        // 1 wECX (1e6 base units at 6 decimals), no fee -> 1e8 sat (still "1 ECX").
        assert_eq!(wecx_base_units_to_ecx_sat(1_000_000, 6, 0), 100_000_000);
    }

    #[test]
    fn pegout_more_wecx_decimals_scales_down() {
        // 1 wECX (1e9 base units at 9 decimals), no fee -> 1e8 sat.
        assert_eq!(wecx_base_units_to_ecx_sat(1_000_000_000, 9, 0), 100_000_000);
    }

    #[test]
    fn pegout_dust_deposit_can_floor_to_zero() {
        assert_eq!(wecx_base_units_to_ecx_sat(1, 9, 0), 0);
    }

    #[test]
    fn pegout_full_fee_zeroes_the_output() {
        assert_eq!(wecx_base_units_to_ecx_sat(1_000_000, 8, 10_000), 0);
    }

    #[test]
    fn same_decimals_no_fee_is_a_straight_passthrough() {
        assert_eq!(ecx_sat_to_wecx_base_units(1_000_000, 8, 0), 1_000_000);
    }

    #[test]
    fn same_decimals_with_fee_applies_flat_bps() {
        // 1_000_000 sat at 5% (500 bps) fee -> 950_000.
        assert_eq!(ecx_sat_to_wecx_base_units(1_000_000, 8, 500), 950_000);
    }

    #[test]
    fn fewer_wecx_decimals_scales_down() {
        // 1 ECX (1e8 sat) with 6-decimal wECX, no fee -> 1e6 base units (still "1 wECX").
        assert_eq!(ecx_sat_to_wecx_base_units(100_000_000, 6, 0), 1_000_000);
    }

    #[test]
    fn more_wecx_decimals_scales_up() {
        // 1 ECX (1e8 sat) with 9-decimal wECX, no fee -> 1e9 base units.
        assert_eq!(ecx_sat_to_wecx_base_units(100_000_000, 9, 0), 1_000_000_000);
    }

    #[test]
    fn dust_deposit_can_floor_to_zero() {
        // 1 sat of ECX (1e-8 ECX) with 6-decimal wECX floors to 0 base units - callers must check
        // for this (see server::create_order) rather than silently accepting a zero-value order.
        assert_eq!(ecx_sat_to_wecx_base_units(1, 6, 0), 0);
    }

    #[test]
    fn oversized_output_saturates_instead_of_wrapping() {
        // u64::MAX sat into a 12-decimal mint scales up by 10^4 - far past u64.
        assert_eq!(ecx_sat_to_wecx_base_units(u64::MAX, 12, 0), u64::MAX);
    }

    #[test]
    fn full_fee_zeroes_the_output() {
        assert_eq!(ecx_sat_to_wecx_base_units(1_000_000, 8, 10_000), 0);
    }

    #[test]
    fn flat_fee_is_removed_before_the_percentage() {
        // 1 ECX, 1% fee, 1_000 sat flat fee. The flat amount is not also charged 1%.
        assert_eq!(pegin_payout_base_units(100_000_000, 8, 100, 1_000), 98_999_010);
        assert_eq!(pegin_payout_base_units(1_000, 8, 100, 1_000), 0);
    }

    #[test]
    fn effective_minimum_covers_the_flat_fee_without_lowering_a_higher_floor() {
        assert_eq!(minimum_pegin_deposit_sat(500, 8, 100, 1_000), 1_002);
        assert_eq!(minimum_pegin_deposit_sat(50_000, 8, 100, 1_000), 50_000);
    }
}
