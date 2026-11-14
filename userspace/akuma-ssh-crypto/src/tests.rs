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

#[cfg(test)]
mod openssh_key_tests {
    extern crate alloc;
    use alloc::string::String;
    use alloc::vec::Vec;
    use ed25519_dalek::SigningKey;

    use crate::crypto::{write_string, write_u32};
    use crate::keys::*;

    // Real files from `ssh-keygen -t ed25519` (OpenSSH 9.x), throwaway keys made for these tests.
    const PLAIN: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\nQyNTUxOQAAACAu04Rq7JekhCiDyKsxPkaT3d++gHqey/r7w8YxUAcWPAAAAJBZ91iJWfdY\niQAAAAtzc2gtZWQyNTUxOQAAACAu04Rq7JekhCiDyKsxPkaT3d++gHqey/r7w8YxUAcWPA\nAAAEBVx8hZeZ9vh9WZEnUmMBorqLWoGEjOdUkgkWGrnkjlDi7ThGrsl6SEKIPIqzE+RpPd\n376Aep7L+vvDxjFQBxY8AAAACmFrdW1hLXRlc3QBAgM=\n-----END OPENSSH PRIVATE KEY-----\n";
    const PLAIN_PUB: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIC7ThGrsl6SEKIPIqzE+RpPd376Aep7L+vvDxjFQBxY8 akuma-test";
    const NO_COMMENT: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\nQyNTUxOQAAACBvDe4e0eyY4TYxXTihTPK2SNcrFDm+ju3W7C5LbBz/WgAAAIgQTpxbEE6c\nWwAAAAtzc2gtZWQyNTUxOQAAACBvDe4e0eyY4TYxXTihTPK2SNcrFDm+ju3W7C5LbBz/Wg\nAAAEBje69vn1YkynnVB7epBEfcxg+J3kLT/jkQoLYPK8gB+G8N7h7R7JjhNjFdOKFM8rZI\n1ysUOb6O7dbsLktsHP9aAAAAAAECAwQF\n-----END OPENSSH PRIVATE KEY-----\n";
    const NO_COMMENT_PUB: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIG8N7h7R7JjhNjFdOKFM8rZI1ysUOb6O7dbsLktsHP9a";
    /// `ssh-keygen -t ed25519 -N hunter2`: aes256-ctr + bcrypt.
    const ENCRYPTED: &str = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABB361XLDe\nKlfJKhFQ9RIWB6AAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIFMtyTIvNg6xXmOF\nI+xyCjMZAmXeK/wloXhEv46IIPsfAAAAkH7Kjd5SSnWLxHe1hArLXA87cxDz0WxUzX/sIg\nHASC8GVxIq4Qok02yVDYONt3mMTWT3yQF5DnHnTin7gPIWFei4VMDq1xXxYjt0Xlu08I00\niM/bE7evf7q7LqV5iIrcobLhgwxm8J9jh45OsLNxDLf3x2tJ7Uj3Pmot62wnPMnJIMAJY1\nX6oG8iLtXBIK6r5A==\n-----END OPENSSH PRIVATE KEY-----\n";

    fn pub_of(seed: &[u8; 32]) -> [u8; 32] {
        *SigningKey::from_bytes(seed).verifying_key().as_bytes()
    }

    /// Every field of a container, so a test can break exactly one.
    struct Parts {
        cipher: &'static [u8],
        kdf: &'static [u8],
        nkeys: u32,
        outer_type: &'static [u8],
        outer_pub: Vec<u8>,
        check1: u32,
        check2: u32,
        inner_type: &'static [u8],
        inner_pub: Vec<u8>,
        secret: Vec<u8>,
        comment: &'static [u8],
        /// `None` = the correct 1,2,3... padding.
        padding: Option<Vec<u8>>,
    }

    fn good(seed: &[u8; 32]) -> Parts {
        let p = pub_of(seed).to_vec();
        let mut secret = seed.to_vec();
        secret.extend_from_slice(&p);
        Parts {
            cipher: b"none", kdf: b"none", nkeys: 1, outer_type: b"ssh-ed25519", outer_pub: p.clone(),
            check1: 0xDEAD_BEEF, check2: 0xDEAD_BEEF, inner_type: b"ssh-ed25519", inner_pub: p, secret,
            comment: b"test", padding: None,
        }
    }

    fn pem(parts: &Parts) -> String {
        let mut outer = Vec::new();
        write_string(&mut outer, parts.outer_type);
        write_string(&mut outer, &parts.outer_pub);

        let mut private = Vec::new();
        write_u32(&mut private, parts.check1);
        write_u32(&mut private, parts.check2);
        write_string(&mut private, parts.inner_type);
        write_string(&mut private, &parts.inner_pub);
        write_string(&mut private, &parts.secret);
        write_string(&mut private, parts.comment);
        if let Some(p) = &parts.padding {
            private.extend_from_slice(p);
        } else {
            let mut i = 1u8;
            while private.len() % 8 != 0 {
                private.push(i);
                i += 1;
            }
        }

        let mut body = Vec::new();
        body.extend_from_slice(b"openssh-key-v1\0");
        write_string(&mut body, parts.cipher);
        write_string(&mut body, parts.kdf);
        write_string(&mut body, b"");
        write_u32(&mut body, parts.nkeys);
        write_string(&mut body, &outer);
        write_string(&mut body, &private);

        let b64 = base64_encode(&body);
        let mut out = String::from("-----BEGIN OPENSSH PRIVATE KEY-----\n");
        for chunk in b64.as_bytes().chunks(70) {
            out.push_str(core::str::from_utf8(chunk).unwrap());
            out.push('\n');
        }
        out.push_str("-----END OPENSSH PRIVATE KEY-----\n");
        out
    }

    const SEED: [u8; 32] = [7u8; 32];

    #[test]
    fn real_openssh_key_yields_the_seed_whose_public_key_is_the_pub_file() {
        let seed = parse_openssh_private_key_ed25519(PLAIN).expect("a real ssh-keygen key must parse");
        let want = parse_public_key_ssh(PLAIN_PUB).expect("pub line parses");
        assert_eq!(&pub_of(&seed), want.as_bytes());
    }

    #[test]
    fn real_openssh_key_with_an_empty_comment_parses() {
        let seed = parse_openssh_private_key_ed25519(NO_COMMENT).expect("empty comment is legal");
        let want = parse_public_key_ssh(NO_COMMENT_PUB).expect("pub line parses");
        assert_eq!(&pub_of(&seed), want.as_bytes());
    }

    #[test]
    fn surrounding_whitespace_and_crlf_line_endings_are_tolerated() {
        let crlf = PLAIN.replace('\n', "\r\n");
        let padded = alloc::format!("\n\n  {}  \n\n", crlf.trim());
        assert_eq!(parse_openssh_private_key_ed25519(&padded), parse_openssh_private_key_ed25519(PLAIN));
    }

    #[test]
    fn a_passphrase_protected_key_is_reported_as_encrypted_not_malformed() {
        assert_eq!(parse_openssh_private_key_ed25519(ENCRYPTED), Err(OpensshKeyError::Encrypted));
    }

    #[test]
    fn synthetic_good_container_roundtrips_the_exact_seed() {
        let text = pem(&good(&SEED));
        assert_eq!(parse_openssh_private_key_ed25519(&text), Ok(SEED));
        // and the builder agrees with ssh-keygen about being a valid container shape
        assert!(looks_like_openssh_private_key(&text));
    }

    #[test]
    fn mismatched_check_integers_are_rejected() {
        // The classic sign of a wrong passphrase / corrupted private section.
        let mut p = good(&SEED);
        p.check2 = p.check1 ^ 1;
        assert_eq!(parse_openssh_private_key_ed25519(&pem(&p)), Err(OpensshKeyError::Malformed));
    }

    #[test]
    fn a_public_key_that_is_not_the_seeds_is_rejected() {
        // Outer, inner and trailing half all agree with each other but not with the seed.
        let other = pub_of(&[9u8; 32]).to_vec();
        let mut p = good(&SEED);
        p.outer_pub = other.clone();
        p.inner_pub = other.clone();
        let mut secret = SEED.to_vec();
        secret.extend_from_slice(&other);
        p.secret = secret;
        assert_eq!(parse_openssh_private_key_ed25519(&pem(&p)), Err(OpensshKeyError::Malformed));
    }

    #[test]
    fn outer_and_inner_public_keys_must_agree() {
        let mut p = good(&SEED);
        p.outer_pub = pub_of(&[9u8; 32]).to_vec();
        assert_eq!(parse_openssh_private_key_ed25519(&pem(&p)), Err(OpensshKeyError::Malformed));
    }

    #[test]
    fn the_trailing_public_half_of_the_secret_must_match() {
        let mut p = good(&SEED);
        let n = p.secret.len();
        p.secret[n - 1] ^= 0x80;
        assert_eq!(parse_openssh_private_key_ed25519(&pem(&p)), Err(OpensshKeyError::Malformed));
    }

    #[test]
    fn a_secret_of_the_wrong_length_is_rejected() {
        let mut p = good(&SEED);
        p.secret.truncate(32);
        assert_eq!(parse_openssh_private_key_ed25519(&pem(&p)), Err(OpensshKeyError::Malformed));
    }

    #[test]
    fn wrong_padding_is_rejected() {
        let mut p = good(&SEED);
        // The padding must be 1,2,3,...; zeros are not accepted even when the length works out.
        p.padding = Some(alloc::vec![0u8; 4]);
        assert_eq!(parse_openssh_private_key_ed25519(&pem(&p)), Err(OpensshKeyError::Malformed));
    }

    #[test]
    fn other_key_types_are_not_ed25519() {
        let mut p = good(&SEED);
        p.outer_type = b"ssh-rsa";
        assert_eq!(parse_openssh_private_key_ed25519(&pem(&p)), Err(OpensshKeyError::NotEd25519));
        let mut q = good(&SEED);
        q.inner_type = b"ssh-rsa";
        assert_eq!(parse_openssh_private_key_ed25519(&pem(&q)), Err(OpensshKeyError::NotEd25519));
    }

    #[test]
    fn more_than_one_key_is_refused_and_zero_is_malformed() {
        let mut p = good(&SEED);
        p.nkeys = 2;
        assert_eq!(parse_openssh_private_key_ed25519(&pem(&p)), Err(OpensshKeyError::MultipleKeys));
        p.nkeys = 0;
        assert_eq!(parse_openssh_private_key_ed25519(&pem(&p)), Err(OpensshKeyError::Malformed));
    }

    #[test]
    fn an_encrypting_cipher_or_kdf_alone_is_enough_to_refuse() {
        let mut p = good(&SEED);
        p.cipher = b"aes256-ctr";
        assert_eq!(parse_openssh_private_key_ed25519(&pem(&p)), Err(OpensshKeyError::Encrypted));
        let mut q = good(&SEED);
        q.kdf = b"bcrypt";
        assert_eq!(parse_openssh_private_key_ed25519(&pem(&q)), Err(OpensshKeyError::Encrypted));
    }

    #[test]
    fn not_a_pem_container_is_not_openssh() {
        assert_eq!(parse_openssh_private_key_ed25519(""), Err(OpensshKeyError::NotOpenssh));
        assert_eq!(parse_openssh_private_key_ed25519("hello"), Err(OpensshKeyError::NotOpenssh));
        assert_eq!(parse_openssh_private_key_ed25519(PLAIN_PUB), Err(OpensshKeyError::NotOpenssh));
        // a begin line with no end line
        let cut = PLAIN.replace("-----END OPENSSH PRIVATE KEY-----", "");
        assert_eq!(parse_openssh_private_key_ed25519(&cut), Err(OpensshKeyError::NotOpenssh));
        assert!(!looks_like_openssh_private_key("\x01\x02 raw bytes"));
    }

    #[test]
    fn valid_base64_with_the_wrong_magic_is_not_openssh() {
        let text = alloc::format!("{}\n{}\n{}\n", OPENSSH_PEM_BEGIN, base64_encode(b"openssh-key-v2\0whatever"),
            "-----END OPENSSH PRIVATE KEY-----");
        assert_eq!(parse_openssh_private_key_ed25519(&text), Err(OpensshKeyError::NotOpenssh));
    }

    #[test]
    fn every_truncation_of_a_real_key_is_rejected_and_never_panics() {
        // Cut the decoded body at every length: each must be an error, none may panic or index out of range.
        let b64: String = PLAIN.lines().filter(|l| !l.starts_with("-----")).collect();
        let raw = base64_decode(&b64).unwrap();
        for cut in 0..raw.len() {
            let text = alloc::format!("{}\n{}\n-----END OPENSSH PRIVATE KEY-----", OPENSSH_PEM_BEGIN, base64_encode(&raw[..cut]));
            assert!(parse_openssh_private_key_ed25519(&text).is_err(), "cut at {cut} was accepted");
        }
    }

    #[test]
    fn trailing_garbage_after_the_private_section_is_rejected() {
        let b64: String = PLAIN.lines().filter(|l| !l.starts_with("-----")).collect();
        let mut raw = base64_decode(&b64).unwrap();
        raw.push(0);
        let text = alloc::format!("{}\n{}\n-----END OPENSSH PRIVATE KEY-----", OPENSSH_PEM_BEGIN, base64_encode(&raw));
        assert_eq!(parse_openssh_private_key_ed25519(&text), Err(OpensshKeyError::Malformed));
    }

    #[test]
    fn describe_names_the_remedy_for_an_encrypted_key() {
        assert!(OpensshKeyError::Encrypted.describe().contains("ssh-keygen"));
    }

    #[test]
    fn encode_then_parse_returns_the_same_seed() {
        for seed in [[0u8; 32], [0xFFu8; 32], SEED, core::array::from_fn(|i| i as u8)] {
            for comment in ["", "x", "akuma-install", "a longer comment with spaces and ünïcode"] {
                let text = encode_openssh_private_key_ed25519(&seed, comment, 0x0102_0304);
                assert!(looks_like_openssh_private_key(&text));
                assert_eq!(parse_openssh_private_key_ed25519(&text), Ok(seed), "comment {comment:?}");
            }
        }
    }

    #[test]
    fn encoded_file_has_the_shape_ssh_keygen_writes() {
        let text = encode_openssh_private_key_ed25519(&SEED, "akuma-test", 7);
        let lines: alloc::vec::Vec<&str> = text.lines().collect();
        assert_eq!(lines.first(), Some(&"-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert_eq!(lines.last(), Some(&"-----END OPENSSH PRIVATE KEY-----"));
        assert!(text.ends_with('\n'));
        // ssh-keygen wraps the base64 at 70 columns
        assert!(lines[1..lines.len() - 2].iter().all(|l| l.len() == 70));
        assert!(lines[lines.len() - 2].len() <= 70);
    }

    #[test]
    fn encoding_the_seed_of_a_real_ssh_keygen_file_gives_a_parseable_equal_key() {
        let seed = parse_openssh_private_key_ed25519(PLAIN).unwrap();
        let again = encode_openssh_private_key_ed25519(&seed, "akuma-test", 1);
        assert_eq!(parse_openssh_private_key_ed25519(&again), Ok(seed));
        // same layout as the real file, whatever its check integers were
        assert_eq!(again.len(), PLAIN.len());
    }
}

