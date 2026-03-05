#[cfg(test)]
mod crypto_tests {
    extern crate alloc;
    use alloc::vec::Vec;
    use crate::crypto::*;

    #[test]
    fn write_read_u32_roundtrip() {
        let mut buf = Vec::new();
        write_u32(&mut buf, 0x1234_5678);
        let mut offset = 0;
        assert_eq!(read_u32(&buf, &mut offset), Some(0x1234_5678));
        assert_eq!(offset, 4);
    }

    #[test]
    fn read_u32_too_short() {
        let buf = [0u8; 3];
        let mut offset = 0;
        assert_eq!(read_u32(&buf, &mut offset), None);
    }

    #[test]
    fn write_read_string_roundtrip() {
        let mut buf = Vec::new();
        write_string(&mut buf, b"hello");
        let mut offset = 0;
        assert_eq!(read_string(&buf, &mut offset), Some(b"hello".as_slice()));
        assert_eq!(offset, 9); // 4-byte len + 5-byte payload
    }

    #[test]
    fn read_string_truncated() {
        let mut buf = Vec::new();
        write_u32(&mut buf, 100); // claim 100 bytes
        buf.push(b'x');           // but only 1 byte present
        let mut offset = 0;
        assert_eq!(read_string(&buf, &mut offset), None);
    }

    #[test]
    fn write_namelist() {
        let mut buf = Vec::new();
        crate::crypto::write_namelist(&mut buf, &["aes128-ctr", "hmac-sha2-256"]);
        let mut offset = 0;
        let s = read_string(&buf, &mut offset).unwrap();
        assert_eq!(s, b"aes128-ctr,hmac-sha2-256");
    }

    #[test]
    fn build_packet_alignment() {
        let payload = b"test";
        let pkt = build_packet(payload);
        // Total length (after the 4-byte length field) must be a multiple of 8
        let mut off = 0;
        let pkt_len = read_u32(&pkt, &mut off).unwrap() as usize;
        assert_eq!(pkt.len(), 4 + pkt_len);
        assert_eq!((1 + payload.len() + (pkt[4] as usize)), pkt_len);
        assert!(pkt[4] >= 4, "padding must be >= 4");
    }

    #[test]
    fn trim_bytes_works() {
        assert_eq!(trim_bytes(b"  hello  "), b"hello");
        assert_eq!(trim_bytes(b""), b"");
        assert_eq!(trim_bytes(b"   "), b"");
        assert_eq!(trim_bytes(b"no_trim"), b"no_trim");
    }

    #[test]
    fn split_first_word_basic() {
        let (a, b) = split_first_word(b"hello world");
        assert_eq!(a, b"hello");
        assert_eq!(b, b"world");
    }

    #[test]
    fn split_first_word_no_space() {
        let (a, b) = split_first_word(b"single");
        assert_eq!(a, b"single");
        assert!(b.is_empty());
    }

    #[test]
    fn simple_rng_deterministic() {
        let mut rng1 = SimpleRng::from_seed([1, 2, 3, 4, 5, 6, 7, 8]);
        let mut rng2 = SimpleRng::from_seed([1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(rng1.next_u64(), rng2.next_u64());
        assert_eq!(rng1.next_u64(), rng2.next_u64());
    }

    #[test]
    fn simple_rng_zero_seed_fallback() {
        let mut rng = SimpleRng::from_seed([0; 8]);
        assert_ne!(rng.next_u64(), 0);
    }

    #[test]
    fn derive_key_known_length() {
        let k = [0u8; 32];
        let h = [1u8; 32];
        let session_id = [2u8; 32];
        let result = derive_key(&k, &h, b'A', &session_id, 16);
        assert_eq!(result.len(), 16);
    }

    #[test]
    fn derive_key_extension() {
        let k = [0xABu8; 32];
        let h = [0xCDu8; 32];
        let session_id = [0xEFu8; 32];
        let result = derive_key(&k, &h, b'A', &session_id, 64);
        assert_eq!(result.len(), 64);
    }

    #[test]
    fn crypto_state_default() {
        let cs = CryptoState::default();
        assert!(cs.decrypt_cipher.is_none());
        assert!(cs.encrypt_cipher.is_none());
        assert_eq!(cs.decrypt_seq, 0);
        assert_eq!(cs.encrypt_seq, 0);
    }
}

#[cfg(test)]
mod keys_tests {
    extern crate alloc;
    use alloc::vec::Vec;
    use crate::keys::*;

    #[test]
    fn base64_roundtrip() {
        let data = b"Hello, world!";
        let encoded = base64_encode(data);
        let decoded = base64_decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn base64_empty() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_decode(""), Some(Vec::new()));
    }

    #[test]
    fn base64_known_vector() {
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_encode(b"M"), "TQ==");
    }

    #[test]
    fn base64_decode_invalid() {
        assert_eq!(base64_decode("!!!"), None); // not multiple of 4
    }

    #[test]
    fn parse_public_key_ssh_comment_line() {
        assert!(parse_public_key_ssh("# this is a comment").is_none());
    }

    #[test]
    fn parse_public_key_ssh_empty() {
        assert!(parse_public_key_ssh("").is_none());
    }

    #[test]
    fn parse_public_key_ssh_unsupported_type() {
        assert!(parse_public_key_ssh("ssh-rsa AAAA user@host").is_none());
    }
}

#[cfg(test)]
mod auth_tests {
    extern crate alloc;
    use alloc::vec::Vec;
    use crate::auth::*;

    #[test]
    fn build_success_response_msg_type() {
        let r = build_success_response();
        assert_eq!(r[0], SSH_MSG_USERAUTH_SUCCESS);
    }

    #[test]
    fn build_failure_response_msg_type() {
        let r = build_failure_response();
        assert_eq!(r[0], SSH_MSG_USERAUTH_FAILURE);
    }

    #[test]
    fn build_pk_ok_response_msg_type() {
        let r = build_pk_ok_response(b"ssh-ed25519", b"keyblob");
        assert_eq!(r[0], SSH_MSG_USERAUTH_PK_OK);
    }

    #[test]
    fn parse_key_blob_wrong_type() {
        use crate::crypto::write_string;
        let mut blob = Vec::new();
        write_string(&mut blob, b"ssh-rsa");
        write_string(&mut blob, &[0u8; 32]);
        assert!(parse_key_blob(&blob).is_none());
    }

    #[test]
    fn parse_key_blob_wrong_length() {
        use crate::crypto::write_string;
        let mut blob = Vec::new();
        write_string(&mut blob, b"ssh-ed25519");
        write_string(&mut blob, &[0u8; 16]); // wrong: 16 instead of 32
        assert!(parse_key_blob(&blob).is_none());
    }

    #[test]
    fn parse_key_blob_rejects_zero_key() {
        use crate::crypto::write_string;
        let mut blob = Vec::new();
        write_string(&mut blob, b"ssh-ed25519");
        write_string(&mut blob, &[0u8; 32]);
        assert!(
            parse_key_blob(&blob).is_none(),
            "identity point (all-zeros key) must be rejected"
        );
    }

    #[test]
    fn parse_key_blob_rejects_low_order_points() {
        use crate::crypto::write_string;
        use crate::auth::LOW_ORDER_POINTS;
        for (i, point) in LOW_ORDER_POINTS.iter().enumerate() {
            let mut blob = Vec::new();
            write_string(&mut blob, b"ssh-ed25519");
            write_string(&mut blob, point);
            assert!(
                parse_key_blob(&blob).is_none(),
                "low-order point {} must be rejected",
                i
            );
        }
    }

    #[test]
    fn verify_signature_valid() {
        use ed25519_dalek::{Signer, SigningKey};
        let signing_key = SigningKey::from_bytes(&[42u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let msg = b"test message";
        let sig = signing_key.sign(msg);
        assert!(verify_signature(&verifying_key, msg, &sig));
    }

    #[test]
    fn verify_signature_wrong_message() {
        use ed25519_dalek::{Signer, SigningKey};
        let signing_key = SigningKey::from_bytes(&[42u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let sig = signing_key.sign(b"correct message");
        assert!(!verify_signature(&verifying_key, b"wrong message", &sig));
    }
}
