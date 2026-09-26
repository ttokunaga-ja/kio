//! Lossless canonical encoding for opaque OS identity components.
//!
//! JCS numbers use IEEE-754 precision. Fixed-width lowercase hexadecimal
//! strings preserve all identity bits without changing numeric comparisons.

use serde::{Deserialize, Deserializer, Serializer, de::Error};

fn decode<'de, D: Deserializer<'de>>(deserializer: D, width: usize) -> Result<u64, D::Error> {
    let value = String::deserialize(deserializer)?;
    if value.len() != width
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(D::Error::custom(format!(
            "expected exactly {width} lowercase hexadecimal digits"
        )));
    }
    u64::from_str_radix(&value, 16).map_err(D::Error::custom)
}

pub mod u64_hex {
    use super::*;

    pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{value:016x}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
        decode(deserializer, 16)
    }
}

pub mod u32_hex {
    use super::*;

    pub fn serialize<S: Serializer>(value: &u32, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&format!("{value:08x}"))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u32, D::Error> {
        u32::try_from(decode(deserializer, 8)?).map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use crate::management::DirectoryIdentity;

    #[test]
    fn identity_jcs_preserves_all_bits_and_adjacent_large_ids() {
        for value in [
            0,
            (1_u64 << 53) + 1,
            0x001d_0000_0001_2345,
            9_851_624_185_183_609,
            u64::MAX,
        ] {
            for identity in [
                DirectoryIdentity::Unix {
                    device: value,
                    inode: value,
                },
                DirectoryIdentity::Windows {
                    volume_serial_number: 0xdead_beef,
                    file_index: value,
                },
            ] {
                let bytes = serde_jcs::to_vec(&identity).unwrap();
                assert_eq!(
                    serde_json::from_slice::<DirectoryIdentity>(&bytes).unwrap(),
                    identity
                );
                assert!(
                    std::str::from_utf8(&bytes)
                        .unwrap()
                        .contains(&format!("\"{value:016x}\""))
                );
            }
        }
        let first = DirectoryIdentity::Windows {
            volume_serial_number: u32::MAX,
            file_index: 1 << 53,
        };
        let next = DirectoryIdentity::Windows {
            volume_serial_number: u32::MAX,
            file_index: (1 << 53) + 1,
        };
        assert_ne!(
            serde_jcs::to_vec(&first).unwrap(),
            serde_jcs::to_vec(&next).unwrap()
        );
    }

    #[test]
    fn identity_rejects_numeric_and_noncanonical_components() {
        for invalid in [
            serde_json::json!(1),
            serde_json::json!(9007199254740993_u64),
            serde_json::json!(null),
            serde_json::json!("1"),
            serde_json::json!("00000000000000000"),
            serde_json::json!("FFFFFFFFFFFFFFFF"),
            serde_json::json!("+000000000000001"),
            serde_json::json!("-000000000000001"),
            serde_json::json!("000000000000000g"),
            serde_json::json!(" 000000000000001"),
        ] {
            for field in ["device", "inode"] {
                let mut json = serde_json::json!({"platform":"unix", "device":"0000000000000001", "inode":"0000000000000001"});
                json[field] = invalid.clone();
                assert!(serde_json::from_value::<DirectoryIdentity>(json).is_err());
            }
            let json = serde_json::json!({"platform":"windows", "volume_serial_number":"deadbeef", "file_index":invalid});
            assert!(serde_json::from_value::<DirectoryIdentity>(json).is_err());
        }
        for invalid in [
            serde_json::json!(1),
            serde_json::json!("1"),
            serde_json::json!("DEADBEEF"),
            serde_json::json!("000000000"),
            serde_json::json!("+0000001"),
            serde_json::json!("-0000001"),
        ] {
            let json = serde_json::json!({"platform":"windows", "volume_serial_number":invalid, "file_index":"0000000000000001"});
            assert!(serde_json::from_value::<DirectoryIdentity>(json).is_err());
        }
    }
}
