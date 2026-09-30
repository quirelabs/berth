//! zstd-compressed bodies inside messages: screens and history.

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::Error;

/// The most a packed body may unpack to. A full screen of wide, styled cells
/// is well under a megabyte and a history range is capped by its sender; the
/// limit stops a small frame from unpacking into gigabytes.
pub const MAX_UNPACKED: usize = 64 * 1024 * 1024;

/// zstd's default. Screens are small and sent often; a higher level costs
/// more time than it saves in bytes.
const LEVEL: i32 = 3;

/// A value, postcard-encoded, then zstd-compressed.
///
/// The compressed bytes are not frozen: another zstd build may compress the
/// same body differently. What is frozen is what they unpack to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Packed(#[serde(with = "serde_bytes_compat")] pub Vec<u8>);

impl Packed {
    pub fn pack<T: Serialize>(value: &T) -> Result<Self, Error> {
        let plain = crate::encode(value, "packed body")?;
        if plain.len() > MAX_UNPACKED {
            return Err(Error::UnpackedTooLarge { max: MAX_UNPACKED });
        }
        zstd::bulk::compress(&plain, LEVEL)
            .map(Packed)
            .map_err(|e| Error::Malformed {
                what: "packed body",
                detail: e.to_string(),
            })
    }

    pub fn unpack<T: DeserializeOwned>(&self) -> Result<T, Error> {
        let plain = zstd::bulk::decompress(&self.0, MAX_UNPACKED).map_err(|e| {
            // zstd reports a body over the capacity as an error like any other;
            // tell the two apart by what the frame header says it holds.
            match zstd::zstd_safe::get_frame_content_size(&self.0) {
                Ok(Some(size)) if size > MAX_UNPACKED as u64 => {
                    Error::UnpackedTooLarge { max: MAX_UNPACKED }
                }
                _ => Error::Malformed {
                    what: "packed body",
                    detail: e.to_string(),
                },
            }
        })?;
        crate::decode_exact(&plain, "packed body")
    }
}

/// `Vec<u8>` as postcard bytes: a length and the bytes, as it would be anyway,
/// spelled out so the encoding does not depend on serde's defaults for
/// sequences changing.
mod serde_bytes_compat {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_bytes(bytes)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let bytes: &[u8] = <&[u8]>::deserialize(d)?;
        Ok(bytes.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_body_comes_back_as_it_went() {
        let body = vec!["line".to_string(); 1000];
        let packed = Packed::pack(&body).unwrap();
        assert!(packed.0.len() < 200, "repetitive text compresses");
        assert_eq!(packed.unpack::<Vec<String>>().unwrap(), body);
    }

    #[test]
    fn garbage_is_malformed_not_a_panic() {
        let packed = Packed(vec![1, 2, 3, 4, 5]);
        assert!(matches!(
            packed.unpack::<Vec<String>>(),
            Err(Error::Malformed { .. })
        ));
    }

    /// A small frame that claims to unpack to a gigabyte is refused, not
    /// allocated.
    #[test]
    fn a_decompression_bomb_is_refused() {
        let zeros = vec![0u8; MAX_UNPACKED + 1];
        let bomb = Packed(zstd::bulk::compress(&zeros, 19).unwrap());
        assert!(bomb.0.len() < 64 * 1024);
        assert_eq!(
            bomb.unpack::<Vec<u8>>(),
            Err(Error::UnpackedTooLarge { max: MAX_UNPACKED })
        );
    }

    /// Unpacks to the right type but with bytes left over.
    #[test]
    fn a_body_with_trailing_bytes_is_refused() {
        let mut plain = postcard::to_stdvec(&7u32).unwrap();
        plain.push(9);
        let packed = Packed(zstd::bulk::compress(&plain, 3).unwrap());
        assert!(matches!(
            packed.unpack::<u32>(),
            Err(Error::Malformed { .. })
        ));
    }
}
