//! Regression tests for the vendored `internal-russh-forked-ssh-key` patch.
//!
//! SAP's dev-tunnel SSH server signs its host key with `ecdsa-sha2-nistp521`
//! and pads each signature integer with a zero byte even when the high bit is
//! already clear. RFC 4251 § 5 forbids that, and upstream `ssh-key` rejects it,
//! which made every `bas_connect` fail with `SshKey: format encoding error`.
//! The patch strips redundant leading zeros instead of failing.
//!
//! These tests exercise the patched parser through `russh`'s re-export, so they
//! fail if the `[patch.crates-io]` override in the workspace manifest is lost.

use russh::keys::ssh_encoding::Decode;
use russh::keys::ssh_key::{Algorithm, EcdsaCurve, Mpint, Signature};

/// Encode one mpint as its SSH wire form: a `u32` big-endian length, then bytes.
fn mpint_wire(bytes: &[u8]) -> Vec<u8> {
    let mut out = (u32::try_from(bytes.len()).expect("mpint length fits u32"))
        .to_be_bytes()
        .to_vec();
    out.extend_from_slice(bytes);
    out
}

#[test]
fn redundant_leading_zero_is_stripped() {
    // A 66-byte p521 field element whose most significant byte is 0x01 must not
    // need a padding zero; SAP sends one anyway.
    let mut field = [0x01u8; 66];
    field[0] = 0x01;
    let padded = [&[0x00u8][..], &field[..]].concat();

    let stripped = Mpint::try_from(padded.as_slice()).expect("padded mpint is accepted");
    let expected = Mpint::try_from(field.as_slice()).expect("plain mpint is accepted");

    assert_eq!(stripped.as_bytes(), field.as_slice(), "padding is removed");
    assert_eq!(stripped, expected, "padded and plain agree");
}

#[test]
fn valid_padding_is_preserved() {
    // high bit set: the leading zero is required by RFC 4251, not redundant.
    let bytes = [0x00u8, 0x80, 0x11, 0x22];
    let mpint = Mpint::try_from(&bytes[..]).expect("needed padding is accepted");

    assert_eq!(mpint.as_bytes(), &bytes[..], "needed padding is kept");
    assert_eq!(mpint.as_positive_bytes(), Some(&bytes[1..]), "value is positive");
}

#[test]
fn zero_gains_its_canonical_empty_encoding() {
    // A standalone 0x00 encodes zero, which RFC 4251 stores as zero bytes.
    let mpint = Mpint::try_from(&[0x00u8][..]).expect("zero is accepted");

    assert!(mpint.as_bytes().is_empty(), "zero has no data bytes");
}

#[test]
fn p521_signature_with_padded_components_parses() {
    // The shape SAP sends: both components carry a redundant zero byte, so each
    // mpint is 67 bytes while the p521 field is 66.
    let r = [0x01u8; 66];
    let s = [0x02u8; 66];
    let padded_r = [&[0x00u8][..], &r[..]].concat();
    let padded_s = [&[0x00u8][..], &s[..]].concat();
    assert_eq!(padded_r.len(), 67, "component carries the padding SAP adds");

    let mut data = mpint_wire(&padded_r);
    data.extend_from_slice(&mpint_wire(&padded_s));

    let signature = Signature::new(
        Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP521,
        },
        data,
    )
    .expect("padded p521 signature is accepted");

    assert_eq!(signature.as_bytes().len(), 67 * 2 + 8, "signature data is kept");
}

#[test]
fn ecdsa_conversion_still_validates_field_size() {
    // Stripping must not turn an oversized component into a valid one. With the
    // required padding in front of a high-bit byte, a p521 element is 67 bytes;
    // two extra value bytes make it 69 and it must still be rejected.
    let oversized = [&[0x00u8, 0x80][..], &[0x03u8; 67][..]].concat();
    assert_eq!(oversized.len(), 69, "padding plus 68 value bytes");

    let mut data = mpint_wire(&oversized);
    data.extend_from_slice(&mpint_wire(&oversized));

    let result = Signature::new(
        Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP521,
        },
        data,
    );

    assert!(result.is_err(), "oversized component is still rejected");
}

#[test]
fn negative_values_keep_their_sign_byte() {
    // `0xff...` is negative; dropping it would turn the value positive.
    let bytes = [0xffu8, 0x21, 0x52];
    let mpint = Mpint::try_from(&bytes[..]).expect("negative mpint is accepted");

    assert_eq!(mpint.as_bytes(), &bytes[..], "sign byte is kept");
    assert_eq!(mpint.as_positive_bytes(), None, "value stays negative");
}

#[test]
fn sap_kex_reply_signature_decodes() {
    // The full shape `russh::client::kex` decodes: a string naming the
    // algorithm, then a string holding the two zero-padded p521 integers.
    let r = [0x00u8, 0x7f, 0x11, 0x22];
    let s = [0x00u8, 0x3e, 0x44, 0x55];
    let mut inner = mpint_wire(&r);
    inner.extend_from_slice(&mpint_wire(&s));

    let mut blob = mpint_wire(b"ecdsa-sha2-nistp521");
    blob.extend_from_slice(&mpint_wire(&inner));

    // Without the patch this fails with `FormatEncoding`, which is the error
    // `bas_connect` reported as "SshKey: format encoding error".
    let signature = Signature::decode(&mut &blob[..]).expect("KEX reply signature decodes");

    assert_eq!(
        signature.algorithm(),
        Algorithm::Ecdsa {
            curve: EcdsaCurve::NistP521,
        },
        "algorithm is preserved"
    );
    assert_eq!(
        signature.as_bytes(),
        inner.as_slice(),
        "signature data keeps the wire encoding"
    );
}
