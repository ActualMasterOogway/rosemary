use sha2::{Digest, Sha256 as InnerSha256};

/// The streaming state of one SHA-256 computation.
#[allow(dead_code)]
pub struct Sha256(InnerSha256);

#[allow(dead_code)]
impl Sha256 {
    pub fn new() -> Self {
        Self(InnerSha256::new())
    }

    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// Absorb a length-prefixed field, so that concatenating two different
    /// splits of the same bytes cannot produce the same digest.
    pub fn update_field(&mut self, bytes: &[u8]) {
        self.update(&(bytes.len() as u64).to_le_bytes());
        self.update(bytes);
    }

    pub fn finish(self) -> [u8; 32] {
        self.0.finalize().into()
    }
}

#[allow(dead_code)]
impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

/// The SHA-256 of one contiguous input.
#[allow(dead_code)]
pub fn digest(bytes: &[u8]) -> [u8; 32] {
    InnerSha256::digest(bytes).into()
}

/// Lowercase hexadecimal, which is how a digest names a file or a directory.
#[allow(dead_code)]
pub fn hex(digest: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::with_capacity(digest.len() * 2);
    for byte in digest {
        text.push(DIGITS[usize::from(byte >> 4)] as char);
        text.push(DIGITS[usize::from(byte & 0xf)] as char);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{digest, hex};

    #[test]
    fn matches_the_published_vectors() {
        assert_eq!(
            hex(&digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&digest(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn a_multi_block_message_hashes_as_one_buffer_does() {
        let message: Vec<u8> = (0..1000u32).map(|value| value as u8).collect();
        let whole = digest(&message);
        let mut split = super::Sha256::new();
        for chunk in message.chunks(7) {
            split.update(chunk);
        }
        assert_eq!(whole, split.finish());
    }

    #[test]
    fn a_million_letters_hash_to_the_published_value() {
        let mut hasher = super::Sha256::new();
        for _ in 0..1000 {
            hasher.update(&[b'a'; 1000]);
        }
        assert_eq!(
            hex(&hasher.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }
}
