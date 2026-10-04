// mantle: tests for `Tls13VectoredSealingKey` (aws-lc-rs#1241; vendor/UPSTREAM.md).
#![cfg(not(feature = "fips"))]

use aws_lc_rs::aead::{
    Aad, Algorithm, Nonce, Tls13VectoredSealingKey, TlsProtocolId, TlsRecordOpeningKey,
    TlsRecordSealingKey, AES_128_GCM, AES_256_GCM, CHACHA20_POLY1305, NONCE_LEN,
};
use aws_lc_rs::test::from_hex;

// The first record the server encrypts in RFC 8448's "Simple 1-RTT Handshake" (§3): four
// handshake messages traced one by one, sealed as one record with sequence number 0 under the
// server's handshake traffic key.
/// `{server} derive write traffic keys for handshake data`: key expanded.
const KEY: &str = "3fce516009c21727d0f2e4e86ee403bc";
/// The same step's iv expanded.
const IV: &str = "5d313eb2671276ee13000b30";
/// `{server} construct an EncryptedExtensions handshake message`.
const ENCRYPTED_EXTENSIONS: &str =
    "080000240022000a00140012001d00170018001901000101010201030104001c0002400100000000";
/// `{server} construct a Certificate handshake message`.
const CERTIFICATE: &str =
    "0b0001b9000001b50001b0308201ac30820115a003020102020102300d06092a864886f70d01010b0500300e\
    310c300a06035504031303727361301e170d3136303733303031323335395a170d3236303733303031323335\
    395a300e310c300a0603550403130372736130819f300d06092a864886f70d010101050003818d0030818902\
    818100b4bb498f8279303d980836399b36c6988c0c68de55e1bdb826d3901a2461eafd2de49a91d015abbc9a\
    95137ace6c1af19eaa6af98c7ced43120998e187a80ee0ccb0524b1b018c3e0b63264d449a6d38e22a5fda43\
    0846748030530ef0461c8ca9d9efbfae8ea6d1d03e2bd193eff0ab9a8002c47428a6d35a8d88d79f7f1e3f02\
    03010001a31a301830090603551d1304023000300b0603551d0f0404030205a0300d06092a864886f70d0101\
    0b05000381810085aad2a0e5b9276b908c65f73a7267170618a54c5f8a7b337d2df7a594365417f2eae8f8a5\
    8c8f8172f9319cf36b7fd6c55b80f21a03015156726096fd335e5e67f2dbf102702e608ccae6bec1fc63a42a\
    99be5c3eb7107c3c54e9b9eb2bd5203b1c3b84e0a8b2f759409ba3eac9d91d402dcc0cc8f8961229ac9187b4\
    2b4de10000";
/// `{server} construct a CertificateVerify handshake message`.
const CERTIFICATE_VERIFY: &str =
    "0f000084080400805a747c5d88fa9bd2e55ab085a61015b7211f824cd484145ab3ff52f1fda8477b0b7abc90\
    db78e2d33a5c141a078653fa6bef780c5ea248eeaaa785c4f394cab6d30bbe8d4859ee511f602957b15411ac\
    027671459e46445c9ea58c181e818e95b8c3fb0bf3278409d3be152a3da5043e063dda65cdf5aea20d53dfac\
    d42f74f3";
/// `{server} construct a Finished handshake message`.
const FINISHED: &str = "140000209b9b141d906337fbd2cbdce71df4deda4ab42c309572cb7fffee5454b78f0718";
/// `{server} send handshake record`: the complete record, header then ciphertext and tag.
const RECORD: &str =
    "17030302a2d1ff334a56f5bff6594a07cc87b580233f500f45e489e7f33af35edf7869fcf40aa40aa2b8ea73\
    f848a7ca07612ef9f945cb960b4068905123ea78b111b429ba9191cd05d2a389280f526134aadc7fc78c4b72\
    9df828b5ecf7b13bd9aefb0e57f271585b8ea9bb355c7c79020716cfb9b1183ef3ab20e37d57a6b9d7477609\
    aee6e122a4cf51427325250c7d0e509289444c9b3a648f1d71035d2ed65b0e3cdd0cbae8bf2d0b227812cbb3\
    60987255cc744110c453baa4fcd610928d809810e4b7ed1a8fd991f06aa6248204797e36a6a73b70a2559c09\
    ead686945ba246ab66e5edd8044b4c6de3fcf2a89441ac66272fd8fb330ef8190579b3684596c960bd596eea\
    520a56a8d650f563aad27409960dca63d3e688611ea5e22f4415cf9538d51a200c27034272968a264ed6540c\
    84838d89f72c24461aad6d26f59ecaba9acbbb317b66d902f4f292a36ac1b639c637ce343117b65962224531\
    7b49eeda0c6258f100d7d961ffb138647e92ea330faeea6dfa31c7a84dc3bd7e1b7a6c7178af36879018e3f2\
    52107f243d243dc7339d5684c8b0378bf30244da8c87c843f5e56eb4c5e8280a2b48052cf93b16499a66db7c\
    ca71e4599426f7d461e66f99882bd89fc50800becca62d6c74116dbd2972fda1fa80f85df881edbe5a376689\
    36b335583b599186dc5c6918a396fa48a181d6b6fa4f9d62d513afbb992f2b992f67f8afe67f76913fa388cb\
    5630c8ca01e0c65d11c66a1e2ac4c85977b7c7a6999bbf10dc35ae69f5515614636c0b9b68c19ed2e31c0b3b\
    66763038ebba42f3b38edc0399f3a9f23faa63978c317fc9fa66a73f60f0504de93b5b845e275592c12335ee\
    340bbc4fddd502784016e4b3be7ef04dda49f4b440a30cb5d2af939828fd4ae3794e44f94df5a631ede42c17\
    19bfdabf0253fe5175be898e750edc53370d2b";
/// The inner content type of a handshake record (RFC 8446 §5.2).
const HANDSHAKE: u8 = 0x16;

const KEY_BYTES: [u8; 32] = [0x42; 32];
const TRAFFIC_IV: [u8; NONCE_LEN] = [
    0x5d, 0x31, 0x3e, 0xb2, 0x67, 0x12, 0x76, 0xee, 0x13, 0x00, 0x0b, 0x30,
];
/// A byte the cipher does not produce for a whole run, so bytes it never wrote show.
const UNWRITTEN: u8 = 0xAA;

fn new_key(algorithm: &'static Algorithm) -> Tls13VectoredSealingKey {
    Tls13VectoredSealingKey::new(algorithm, &KEY_BYTES[..algorithm.key_len()], &TRAFFIC_IV).unwrap()
}

/// RFC 8446 §5.3's nonce for `sequence`, as a caller of `TlsRecordSealingKey` builds it.
fn nonce(sequence: u64) -> Nonce {
    let mut n = TRAFFIC_IV;
    for (b, s) in n[NONCE_LEN - 8..].iter_mut().zip(sequence.to_be_bytes()) {
        *b ^= s;
    }
    Nonce::assume_unique_for_key(n)
}

fn pattern(len: usize, seed: usize) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from((i * 7 + seed) % 251).unwrap())
        .collect()
}

/// `data` cut at `cuts` (offsets in any order, taken modulo the length).
fn pieces<'a>(data: &'a [u8], cuts: &[usize]) -> Vec<&'a [u8]> {
    let mut at: Vec<usize> = cuts.iter().map(|c| c % (data.len() + 1)).collect();
    at.push(0);
    at.push(data.len());
    at.sort_unstable();
    at.windows(2).map(|w| &data[w[0]..w[1]]).collect()
}

#[test]
fn rfc_8448_server_handshake_record() {
    let messages: Vec<Vec<u8>> = [
        ENCRYPTED_EXTENSIONS,
        CERTIFICATE,
        CERTIFICATE_VERIFY,
        FINISHED,
    ]
    .iter()
    .map(|h| from_hex(h).unwrap())
    .collect();
    let record = from_hex(RECORD).unwrap();
    let (header, sealed) = record.split_at(5);
    let mut slices: Vec<&[u8]> = messages.iter().map(Vec::as_slice).collect();
    slices.push(&[HANDSHAKE]);
    let len: usize = slices.iter().map(|s| s.len()).sum();

    let mut key = Tls13VectoredSealingKey::new(
        &AES_128_GCM,
        &from_hex(KEY).unwrap(),
        &from_hex(IV).unwrap(),
    )
    .unwrap();
    let mut out = vec![UNWRITTEN; len + 16 + 3];
    key.seal_vectored(0, Aad::from(header), slices.iter().copied(), len, &mut out)
        .unwrap();
    assert_eq!(&out[..len + 16], sealed);
    assert_eq!(&out[len + 16..], &[UNWRITTEN; 3], "wrote past the record");

    // The same record from the same plaintext cut elsewhere, appended after a prefix.
    let whole: Vec<u8> = slices.concat();
    let mut key = Tls13VectoredSealingKey::new(
        &AES_128_GCM,
        &from_hex(KEY).unwrap(),
        &from_hex(IV).unwrap(),
    )
    .unwrap();
    let mut appended = header.to_vec();
    key.seal_vectored_append(
        0,
        Aad::from(header),
        pieces(&whole, &[1, 15, 16, 17, 300, 657]),
        len,
        &mut appended,
    )
    .unwrap();
    assert_eq!(appended, record);
}

#[test]
fn records_match_tls_record_sealing_key() {
    for algorithm in [&AES_128_GCM, &AES_256_GCM] {
        let mut vectored = new_key(algorithm);
        let mut contiguous = TlsRecordSealingKey::new(
            algorithm,
            TlsProtocolId::TLS13,
            &KEY_BYTES[..algorithm.key_len()],
        )
        .unwrap();
        let opening = TlsRecordOpeningKey::new(
            algorithm,
            TlsProtocolId::TLS13,
            &KEY_BYTES[..algorithm.key_len()],
        )
        .unwrap();
        let cases: [(usize, &[usize]); 8] = [
            (0, &[]),
            (1, &[0, 1]),
            (15, &[3, 3, 3]),
            (16, &[16]),
            (17, &[1, 16]),
            (1000, &[5, 250, 251, 999]),
            (16385, &[1, 4096, 8191, 16384]),
            (16640, &[13, 26, 39, 16639]),
        ];
        for (sequence, (len, cuts)) in (0u64..).zip(cases) {
            let plaintext = pattern(len, usize::try_from(sequence).unwrap());
            let header = [0x17, 0x03, 0x03, 0x40, 0x11];

            let mut expected = plaintext.clone();
            contiguous
                .seal_in_place_append_tag(nonce(sequence), Aad::from(header), &mut expected)
                .unwrap();

            let mut out = vec![UNWRITTEN; len + algorithm.tag_len()];
            vectored
                .seal_vectored(
                    sequence,
                    Aad::from(header),
                    pieces(&plaintext, cuts),
                    len,
                    &mut out,
                )
                .unwrap();
            assert_eq!(out, expected, "{algorithm:?} len={len}");

            let opened = opening
                .open_in_place(nonce(sequence), Aad::from(header), &mut out)
                .unwrap();
            assert_eq!(opened, plaintext.as_slice());
        }
    }
}

#[test]
fn sequence_numbers_strictly_increase_and_never_wrap() {
    let mut key = new_key(&AES_128_GCM);
    let mut out = [0u8; 17];
    let mut seal = |key: &mut Tls13VectoredSealingKey, sequence: u64| {
        key.seal_vectored(sequence, Aad::empty(), [&b"x"[..]], 1, &mut out)
    };
    assert!(seal(&mut key, 0).is_ok());
    assert!(
        seal(&mut key, 0).is_err(),
        "a sequence number is sealed once"
    );
    assert!(seal(&mut key, 5).is_ok(), "numbers may skip");
    assert!(seal(&mut key, 3).is_err(), "numbers only increase");
    assert!(
        seal(&mut key, 6).is_ok(),
        "a refused number leaves the key usable"
    );
    assert!(seal(&mut key, u64::MAX).is_err(), "the number would wrap");
    assert!(seal(&mut key, u64::MAX - 1).is_ok());
    assert!(seal(&mut key, u64::MAX).is_err());
}

#[test]
fn refusals_before_encrypting_leave_output_and_key_alone() {
    let mut key = new_key(&AES_256_GCM);
    let plaintext = pattern(100, 1);
    let mut short = vec![UNWRITTEN; 100 + 15];
    assert!(key
        .seal_vectored(0, Aad::empty(), [plaintext.as_slice()], 100, &mut short)
        .is_err());
    assert!(short.iter().all(|&b| b == UNWRITTEN));
    // The refused seal did not spend sequence number 0.
    let mut out = vec![UNWRITTEN; 116];
    key.seal_vectored(0, Aad::empty(), [plaintext.as_slice()], 100, &mut out)
        .unwrap();

    assert!(key
        .seal_vectored(
            1,
            Aad::empty(),
            [plaintext.as_slice()],
            usize::MAX,
            &mut out
        )
        .is_err());
    key.seal_vectored(1, Aad::empty(), [plaintext.as_slice()], 100, &mut out)
        .unwrap();
}

#[test]
fn slices_that_do_not_add_up_fail_the_key_without_writing_past_the_record() {
    let plaintext = pattern(64, 2);
    // More than declared: the extra slice is refused before it is written.
    let mut key = new_key(&AES_128_GCM);
    let mut out = vec![UNWRITTEN; 64 + 16 + 8];
    let too_many = [&plaintext[..40], &plaintext[40..], &plaintext[..8]];
    assert!(key
        .seal_vectored(0, Aad::empty(), too_many, 64, &mut out)
        .is_err());
    assert!(
        out[64..].iter().all(|&b| b == UNWRITTEN),
        "wrote past the plaintext"
    );
    assert!(
        key.seal_vectored(1, Aad::empty(), [plaintext.as_slice()], 64, &mut out)
            .is_err(),
        "a key whose seal failed partway refuses every later seal"
    );

    // Fewer than declared.
    let mut key = new_key(&AES_128_GCM);
    let mut out = vec![UNWRITTEN; 64 + 16];
    assert!(key
        .seal_vectored(0, Aad::empty(), [&plaintext[..63]], 64, &mut out)
        .is_err());
    assert!(out[63..].iter().all(|&b| b == UNWRITTEN));
    assert!(key
        .seal_vectored(1, Aad::empty(), [plaintext.as_slice()], 64, &mut out)
        .is_err());
}

#[test]
fn a_seal_the_plaintext_iterator_abandons_fails_the_key() {
    let mut key = new_key(&AES_128_GCM);
    let plaintext = pattern(32, 3);
    let mut out = vec![0u8; 48];
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let slices = [&plaintext[..16], &plaintext[16..]].into_iter().map(|s| {
            if s[0] == plaintext[16] {
                panic!("the caller's iterator fails partway");
            }
            s
        });
        let _ = key.seal_vectored(0, Aad::empty(), slices, 32, &mut out);
    }));
    assert!(panicked.is_err());
    assert!(key
        .seal_vectored(1, Aad::empty(), [plaintext.as_slice()], 32, &mut out)
        .is_err());
}

#[test]
fn appending_keeps_the_prefix_and_exposes_only_sealed_bytes() {
    let plaintext = pattern(300, 4);
    let mut key = new_key(&AES_128_GCM);
    let mut out = b"prefix".to_vec();
    out.shrink_to_fit();
    key.seal_vectored_append(
        0,
        Aad::empty(),
        pieces(&plaintext, &[7, 150]),
        300,
        &mut out,
    )
    .unwrap();
    assert_eq!(out.len(), 6 + 300 + 16);
    assert_eq!(&out[..6], b"prefix");

    // A failed seal leaves the vector as it was.
    let before = out.clone();
    assert!(key
        .seal_vectored_append(1, Aad::empty(), [&plaintext[..10]], 300, &mut out)
        .is_err());
    assert_eq!(out, before);

    // The appended record opens.
    let opening =
        TlsRecordOpeningKey::new(&AES_128_GCM, TlsProtocolId::TLS13, &KEY_BYTES[..16]).unwrap();
    let mut sealed = before[6..].to_vec();
    let opened = opening
        .open_in_place(nonce(0), Aad::empty(), &mut sealed)
        .unwrap();
    assert_eq!(opened, plaintext.as_slice());
}

#[test]
fn keys_other_than_aes_gcm_with_a_12_byte_iv_are_refused() {
    assert!(Tls13VectoredSealingKey::new(&CHACHA20_POLY1305, &KEY_BYTES, &TRAFFIC_IV).is_err());
    assert!(Tls13VectoredSealingKey::new(&AES_128_GCM, &KEY_BYTES, &TRAFFIC_IV).is_err());
    assert!(
        Tls13VectoredSealingKey::new(&AES_128_GCM, &KEY_BYTES[..16], &TRAFFIC_IV[..8]).is_err()
    );
    assert!(Tls13VectoredSealingKey::new(&AES_256_GCM, &KEY_BYTES, &TRAFFIC_IV).is_ok());
}
