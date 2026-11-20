use super::Type;

/// Semantic integer layout shared by checking, proofs and native emission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IntegerLayout {
    pub bits: u32,
    pub signed: bool,
}

impl IntegerLayout {
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
    pub fn integer_layout(&self) -> Option<IntegerLayout> {
        match self {
            Self::I64 => Some(IntegerLayout {
                bits: 64,
                signed: true,
            }),
            Self::U8 => Some(IntegerLayout {
                bits: 8,
                signed: false,
            }),
            _ => None,
        }
    }
}
