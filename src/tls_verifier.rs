//! Custom X.509 Certificate Verifier for TLS
//!
//! Implements the `TlsVerifier` trait using RustCrypto's `x509-cert` for certificate parsing
//! and `p256`/`ed25519-dalek` for signature verification.
//!
//! Supports:
//! - ECDSA P-256 with SHA-256
//! - Ed25519
//!
//! Limitations:
//! - RSA signatures not supported (would need `rsa` crate)
//! - No chain validation (trusts end-entity cert directly)
//! - No revocation checking (CRL/OCSP)

use alloc::string::String;
use alloc::vec::Vec;
use core::marker::PhantomData;

use der::Decode;
use embedded_tls::extensions::extension_data::signature_algorithms::SignatureScheme;
use embedded_tls::handshake::certificate::{CertificateEntryRef, CertificateRef};
use embedded_tls::handshake::certificate_verify::CertificateVerify;
use embedded_tls::{Certificate, TlsCipherSuite, TlsError, TlsVerifier};
use sha2::Digest;
use x509_cert::Certificate as X509Certificate;

use crate::timer;

// ============================================================================
// X.509 Verifier
// ============================================================================

/// Custom X.509 certificate verifier
///
/// Verifies server certificates using x509-cert for parsing and
/// p256/ed25519-dalek for signature verification.
pub struct X509Verifier<'a, CipherSuite>
where
    CipherSuite: TlsCipherSuite,
{
    /// Server hostname for verification
    host: Option<&'a str>,
    /// Stored certificate DER for signature verification
    certificate_der: Option<Vec<u8>>,
    /// Transcript hash at certificate verification time
    transcript_hash: Option<Vec<u8>>,
    /// Phantom data for cipher suite
    _cipher: PhantomData<CipherSuite>,
}

impl<'a, CipherSuite> TlsVerifier<'a, CipherSuite> for X509Verifier<'a, CipherSuite>
where
    CipherSuite: TlsCipherSuite,
{
    fn new(host: Option<&'a str>) -> Self {
        Self {
            host,
            certificate_der: None,
            transcript_hash: None,
            _cipher: PhantomData,
        }
    }

    fn verify_certificate(
        &mut self,
        transcript: &CipherSuite::Hash,
        _ca: &Option<Certificate>,
        cert: CertificateRef,
    ) -> Result<(), TlsError> {
        // Get the first certificate (end-entity)
        if cert.entries.is_empty() {
            return Err(TlsError::InvalidCertificate);
        }

        let cert_der = match &cert.entries[0] {
            CertificateEntryRef::X509(der) => *der,
            _ => return Err(TlsError::InvalidCertificate),
        };

        // Parse the X.509 certificate
        let x509 = X509Certificate::from_der(cert_der).map_err(|_| TlsError::InvalidCertificate)?;

        // Check validity dates if we have system time
        if let Some(now_secs) = timer::utc_seconds() {
            let validity = &x509.tbs_certificate.validity;
            // Note: Proper time comparison would require parsing ASN.1 time
            let _ = (validity, now_secs); // TODO: Implement proper time comparison
        }

        // Verify hostname if provided
        if let Some(expected_host) = self.host {
            if !verify_hostname(&x509, expected_host) {
                return Err(TlsError::InvalidCertificate);
            }
        }

        // Store certificate and transcript for signature verification
        self.certificate_der = Some(cert_der.to_vec());

        // Clone the transcript hash
        let hash = transcript.clone().finalize();
        self.transcript_hash = Some(hash.to_vec());

        Ok(())
    }

    fn verify_signature(&mut self, verify: CertificateVerify) -> Result<(), TlsError> {
        let cert_der = self
            .certificate_der
            .as_ref()
            .ok_or(TlsError::InvalidCertificate)?;

        let transcript_hash = self
            .transcript_hash
            .as_ref()
            .ok_or(TlsError::InvalidCertificate)?;

        // Parse certificate to get public key
        let x509 =
            X509Certificate::from_der(cert_der).map_err(|_| TlsError::InvalidCertificate)?;

        // Build the TLS 1.3 CertificateVerify message content
        // See RFC 8446 Section 4.4.3
        let mut message = Vec::with_capacity(64 + 33 + 1 + transcript_hash.len());
        message.resize(64, 0x20u8); // 64 spaces
        message.extend_from_slice(b"TLS 1.3, server CertificateVerify\x00");
        message.extend_from_slice(transcript_hash);

        // Verify based on signature scheme
        match verify.signature_scheme {
            SignatureScheme::EcdsaSecp256r1Sha256 => {
                verify_ecdsa_p256(&x509, &message, &verify.signature)
            }
            SignatureScheme::Ed25519 => verify_ed25519(&x509, &message, &verify.signature),
            SignatureScheme::RsaPkcs1Sha256 => {
                verify_rsa_pkcs1_sha256(&x509, &message, &verify.signature)
            }
            SignatureScheme::RsaPkcs1Sha384 => {
                verify_rsa_pkcs1_sha384(&x509, &message, &verify.signature)
            }
            SignatureScheme::RsaPkcs1Sha512 => {
                verify_rsa_pkcs1_sha512(&x509, &message, &verify.signature)
            }
            SignatureScheme::RsaPssRsaeSha256 => {
                verify_rsa_pss_sha256(&x509, &message, &verify.signature)
            }
            SignatureScheme::RsaPssRsaeSha384 => {
                verify_rsa_pss_sha384(&x509, &message, &verify.signature)
            }
            SignatureScheme::RsaPssRsaeSha512 => {
                verify_rsa_pss_sha512(&x509, &message, &verify.signature)
            }
            _ => Err(TlsError::InvalidSignatureScheme),
        }
    }
}

// ============================================================================
// Hostname Verification
// ============================================================================

/// Verify that the certificate is valid for the given hostname
fn verify_hostname(cert: &X509Certificate, hostname: &str) -> bool {
    use x509_cert::ext::pkix::name::GeneralName;
    use x509_cert::ext::pkix::SubjectAltName;

    // Try to get Subject Alternative Names extension
    if let Some(extensions) = &cert.tbs_certificate.extensions {
        for ext in extensions.iter() {
            // Check if this is the SAN extension (OID 2.5.29.17)
            if ext.extn_id == const_oid::db::rfc5280::ID_CE_SUBJECT_ALT_NAME {
                if let Ok(san) = SubjectAltName::from_der(ext.extn_value.as_bytes()) {
                    for name in san.0.iter() {
                        match name {
                            GeneralName::DnsName(dns_name) => {
                                if matches_hostname(dns_name.as_str(), hostname) {
                                    return true;
                                }
                            }
                            GeneralName::IpAddress(ip_bytes) => {
                                if ip_bytes.as_bytes().len() == 4 {
                                    let bytes = ip_bytes.as_bytes();
                                    let ip_str = alloc::format!(
                                        "{}.{}.{}.{}",
                                        bytes[0], bytes[1], bytes[2], bytes[3]
                                    );
                                    if ip_str == hostname {
                                        return true;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    // Fall back to Common Name (CN) in subject
    for rdn in cert.tbs_certificate.subject.0.iter() {
        for atv in rdn.0.iter() {
            // Check for Common Name OID (2.5.4.3)
            if atv.oid == const_oid::db::rfc4519::CN {
                if let Ok(cn) = core::str::from_utf8(atv.value.value()) {
                    if matches_hostname(cn, hostname) {
                        return true;
                    }
                }
            }
        }
    }

    false
}

/// Check if a certificate name matches the hostname (supports wildcards)
fn matches_hostname(cert_name: &str, hostname: &str) -> bool {
    let cert_lower = to_lowercase(cert_name);
    let host_lower = to_lowercase(hostname);

    if cert_lower.starts_with("*.") {
        // Wildcard match: *.example.com matches foo.example.com
        let suffix = &cert_lower[2..];
        if let Some(pos) = host_lower.find('.') {
            return &host_lower[pos + 1..] == suffix;
        }
        false
    } else {
        cert_lower == host_lower
    }
}

/// Convert to lowercase (no_std compatible)
fn to_lowercase(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_uppercase() {
                (c as u8 + 32) as char
            } else {
                c
            }
        })
        .collect()
}

// ============================================================================
// Signature Verification
// ============================================================================

/// Verify ECDSA P-256 signature
fn verify_ecdsa_p256(
    cert: &X509Certificate,
    message: &[u8],
    signature: &[u8],
) -> Result<(), TlsError> {
    use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};

    // Get the public key from the certificate's SubjectPublicKeyInfo
    let spki = &cert.tbs_certificate.subject_public_key_info;
    let key_bytes = spki.subject_public_key.raw_bytes();

    // Parse the verifying key (expects SEC1 encoded point)
    let verifying_key =
        VerifyingKey::from_sec1_bytes(key_bytes).map_err(|_| TlsError::InvalidCertificate)?;

    // Parse the DER-encoded signature
    let sig = Signature::from_der(signature).map_err(|_| TlsError::InvalidSignature)?;

    // Verify the signature over the message
    verifying_key
        .verify(message, &sig)
        .map_err(|_| TlsError::InvalidSignature)?;

    Ok(())
}

/// Verify Ed25519 signature
fn verify_ed25519(
    cert: &X509Certificate,
    message: &[u8],
    signature: &[u8],
) -> Result<(), TlsError> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    // Get the public key from the certificate's SubjectPublicKeyInfo
    let spki = &cert.tbs_certificate.subject_public_key_info;
    let key_bytes = spki.subject_public_key.raw_bytes();

    // Ed25519 public keys are 32 bytes
    if key_bytes.len() != 32 {
        return Err(TlsError::InvalidCertificate);
    }

    let key_array: [u8; 32] = key_bytes.try_into().map_err(|_| TlsError::InvalidCertificate)?;
    let verifying_key =
        VerifyingKey::from_bytes(&key_array).map_err(|_| TlsError::InvalidCertificate)?;

    // Ed25519 signatures are 64 bytes
    if signature.len() != 64 {
        return Err(TlsError::InvalidSignature);
    }

    let sig_array: [u8; 64] = signature.try_into().map_err(|_| TlsError::InvalidSignature)?;
    let sig = Signature::from_bytes(&sig_array);

    // Verify signature
    verifying_key
        .verify(message, &sig)
        .map_err(|_| TlsError::InvalidSignature)?;

    Ok(())
}

/// Verify RSA PKCS#1 v1.5 signature with SHA-256
fn verify_rsa_pkcs1_sha256(
    cert: &X509Certificate,
    message: &[u8],
    signature: &[u8],
) -> Result<(), TlsError> {
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::signature::Verifier;
    use rsa::RsaPublicKey;
    use rsa::pkcs8::DecodePublicKey;

    // Get the public key from the certificate's SubjectPublicKeyInfo
    let spki = &cert.tbs_certificate.subject_public_key_info;
    let spki_der = der::Encode::to_der(spki).map_err(|_| TlsError::InvalidCertificate)?;

    // Parse RSA public key from DER
    let public_key =
        RsaPublicKey::from_public_key_der(&spki_der).map_err(|_| TlsError::InvalidCertificate)?;

    // Create verifying key
    let verifying_key = VerifyingKey::<sha2::Sha256>::new(public_key);

    // Parse signature
    let sig = Signature::try_from(signature).map_err(|_| TlsError::InvalidSignature)?;

    // Verify
    verifying_key
        .verify(message, &sig)
        .map_err(|_| TlsError::InvalidSignature)?;

    Ok(())
}

/// Verify RSA PKCS#1 v1.5 signature with SHA-384
fn verify_rsa_pkcs1_sha384(
    cert: &X509Certificate,
    message: &[u8],
    signature: &[u8],
) -> Result<(), TlsError> {
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::signature::Verifier;
    use rsa::RsaPublicKey;
    use rsa::pkcs8::DecodePublicKey;

    let spki = &cert.tbs_certificate.subject_public_key_info;
    let spki_der = der::Encode::to_der(spki).map_err(|_| TlsError::InvalidCertificate)?;
    let public_key =
        RsaPublicKey::from_public_key_der(&spki_der).map_err(|_| TlsError::InvalidCertificate)?;
    let verifying_key = VerifyingKey::<sha2::Sha384>::new(public_key);
    let sig = Signature::try_from(signature).map_err(|_| TlsError::InvalidSignature)?;
    verifying_key
        .verify(message, &sig)
        .map_err(|_| TlsError::InvalidSignature)?;
    Ok(())
}

/// Verify RSA PKCS#1 v1.5 signature with SHA-512
fn verify_rsa_pkcs1_sha512(
    cert: &X509Certificate,
    message: &[u8],
    signature: &[u8],
) -> Result<(), TlsError> {
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::signature::Verifier;
    use rsa::RsaPublicKey;
    use rsa::pkcs8::DecodePublicKey;

    let spki = &cert.tbs_certificate.subject_public_key_info;
    let spki_der = der::Encode::to_der(spki).map_err(|_| TlsError::InvalidCertificate)?;
    let public_key =
        RsaPublicKey::from_public_key_der(&spki_der).map_err(|_| TlsError::InvalidCertificate)?;
    let verifying_key = VerifyingKey::<sha2::Sha512>::new(public_key);
    let sig = Signature::try_from(signature).map_err(|_| TlsError::InvalidSignature)?;
    verifying_key
        .verify(message, &sig)
        .map_err(|_| TlsError::InvalidSignature)?;
    Ok(())
}

/// Verify RSA-PSS signature with SHA-256
fn verify_rsa_pss_sha256(
    cert: &X509Certificate,
    message: &[u8],
    signature: &[u8],
) -> Result<(), TlsError> {
    use rsa::pss::{Signature, VerifyingKey};
    use rsa::signature::Verifier;
    use rsa::RsaPublicKey;
    use rsa::pkcs8::DecodePublicKey;

    let spki = &cert.tbs_certificate.subject_public_key_info;
    let spki_der = der::Encode::to_der(spki).map_err(|_| TlsError::InvalidCertificate)?;
    let public_key =
        RsaPublicKey::from_public_key_der(&spki_der).map_err(|_| TlsError::InvalidCertificate)?;
    let verifying_key = VerifyingKey::<sha2::Sha256>::new(public_key);
    let sig = Signature::try_from(signature).map_err(|_| TlsError::InvalidSignature)?;
    verifying_key
        .verify(message, &sig)
        .map_err(|_| TlsError::InvalidSignature)?;
    Ok(())
}

/// Verify RSA-PSS signature with SHA-384
fn verify_rsa_pss_sha384(
    cert: &X509Certificate,
    message: &[u8],
    signature: &[u8],
) -> Result<(), TlsError> {
    use rsa::pss::{Signature, VerifyingKey};
    use rsa::signature::Verifier;
    use rsa::RsaPublicKey;
    use rsa::pkcs8::DecodePublicKey;

    let spki = &cert.tbs_certificate.subject_public_key_info;
    let spki_der = der::Encode::to_der(spki).map_err(|_| TlsError::InvalidCertificate)?;
    let public_key =
        RsaPublicKey::from_public_key_der(&spki_der).map_err(|_| TlsError::InvalidCertificate)?;
    let verifying_key = VerifyingKey::<sha2::Sha384>::new(public_key);
    let sig = Signature::try_from(signature).map_err(|_| TlsError::InvalidSignature)?;
    verifying_key
        .verify(message, &sig)
        .map_err(|_| TlsError::InvalidSignature)?;
    Ok(())
}

/// Verify RSA-PSS signature with SHA-512
fn verify_rsa_pss_sha512(
    cert: &X509Certificate,
    message: &[u8],
    signature: &[u8],
) -> Result<(), TlsError> {
    use rsa::pss::{Signature, VerifyingKey};
    use rsa::signature::Verifier;
    use rsa::RsaPublicKey;
    use rsa::pkcs8::DecodePublicKey;

    let spki = &cert.tbs_certificate.subject_public_key_info;
    let spki_der = der::Encode::to_der(spki).map_err(|_| TlsError::InvalidCertificate)?;
    let public_key =
        RsaPublicKey::from_public_key_der(&spki_der).map_err(|_| TlsError::InvalidCertificate)?;
    let verifying_key = VerifyingKey::<sha2::Sha512>::new(public_key);
    let sig = Signature::try_from(signature).map_err(|_| TlsError::InvalidSignature)?;
    verifying_key
        .verify(message, &sig)
        .map_err(|_| TlsError::InvalidSignature)?;
    Ok(())
}
