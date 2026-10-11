//! Lowercase hexadecimal formatting without an intermediate allocation.
pub(crate) struct Hex<'a>(pub(crate) &'a [u8]);

impl std::fmt::LowerHex for Hex<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::Hex;
    use sha2::{Digest, Sha256};

    #[test]
    fn preserves_sha256_known_vectors_and_streaming() {
        for (input, expected) in [
            (
                "",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                "abc",
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
        ] {
            assert_eq!(format!("{:x}", Hex(&Sha256::digest(input))), expected);
            let mut digest = Sha256::new();
            for byte in input.bytes() {
                digest.update([byte]);
            }
            assert_eq!(format!("{:x}", Hex(&digest.finalize())), expected);
        }
    }

    #[test]
    fn preserves_leading_zeros_and_every_byte() {
        assert_eq!(format!("{:x}", Hex(&[])), "");
        assert_eq!(format!("{:x}", Hex(&[0, 1, 15, 16, 255])), "00010f10ff");
        let bytes: Vec<u8> = (0..=255).collect();
        let output = format!("{:x}", Hex(&bytes));
        assert_eq!(output.len(), bytes.len() * 2);
        for (expected, pair) in bytes.iter().zip(output.as_bytes().chunks_exact(2)) {
            assert_eq!(
                u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap(),
                *expected
            );
        }
    }

    #[test]
    fn output_work_is_linear_for_increasing_inputs() {
        use std::fmt::Write;
        #[derive(Default)]
        struct Counter {
            writes: usize,
            bytes: usize,
        }
        impl Write for Counter {
            fn write_str(&mut self, value: &str) -> std::fmt::Result {
                self.writes += 1;
                self.bytes += value.len();
                Ok(())
            }
        }
        for size in [0, 1, 2, 32, 256, 4096] {
            let input = vec![0; size];
            let mut counter = Counter::default();
            write!(counter, "{:x}", Hex(&input)).unwrap();
            assert_eq!(counter.bytes, 2 * size);
            assert!(counter.writes <= 2 * size);
            println!(
                "hex input={size} output={} writes={}",
                counter.bytes, counter.writes
            );
        }
    }
}
