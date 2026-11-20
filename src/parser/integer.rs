use super::Type;

/// Semantic integer layout shared by checking, proofs and native emission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntegerLayout {
    pub bits: u32,
    pub signed: bool,
}

impl IntegerLayout {
    /// Decode the canonical 64-bit payload after source range checking.
    pub fn value(self, bits: i64) -> i128 {
        if !self.signed && self.bits == 64 {
            i128::from(bits as u64)
        } else {
            i128::from(bits)
        }
    }

    pub fn min(self) -> i128 {
        if self.signed {
            -(1_i128 << (self.bits - 1))
        } else {
            0
        }
    }
    pub fn max(self) -> i128 {
        (1_i128 << (self.bits - u32::from(self.signed))) - 1
    }
    pub fn contains(self, value: i128) -> bool {
        (self.min()..=self.max()).contains(&value)
    }
    pub fn wrap(self, value: i128) -> i64 {
        let shift = 128 - self.bits;
        if self.signed {
            ((value << shift) >> shift) as i64
        } else {
            (value & self.max()) as i64
        }
    }
}

impl<N> Type<N> {
    /// Storage/conversion range. Unicode validation remains separate from integers.
    pub fn scalar_layout(&self) -> Option<IntegerLayout> {
        if matches!(self, Self::Char) {
            Some(IntegerLayout {
                bits: 32,
                signed: false,
            })
        } else {
            self.integer_layout()
        }
    }
    pub fn scalar_width(&self) -> u32 {
        self.scalar_layout().map_or(8, |layout| layout.bits / 8)
    }

    pub fn integer_layout(&self) -> Option<IntegerLayout> {
        let (bits, signed) = match self {
            Self::I8 => (8, true),
            Self::I16 => (16, true),
            Self::I32 => (32, true),
            Self::I64 => (64, true),
            Self::U8 => (8, false),
            Self::U16 => (16, false),
            Self::U32 => (32, false),
            Self::U64 => (64, false),
            _ => return None,
        };
        Some(IntegerLayout { bits, signed })
    }
}
