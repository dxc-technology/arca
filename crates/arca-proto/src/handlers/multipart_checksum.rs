//! The checksum of a completed multipart upload (TD-046).
//!
//! S3 gives a multipart object one of two kinds of checksum, chosen when the
//! upload is created (`x-amz-checksum-type`):
//!
//! | Type | Value | Algorithms |
//! |------|-------|------------|
//! | `COMPOSITE` | `base64(H(raw part checksums, concatenated))-N`, `N` the number of parts | CRC32, CRC32C, SHA1, SHA256 |
//! | `FULL_OBJECT` | the CRC of the whole object, rebuilt from the part CRCs and sizes | CRC32, CRC32C, CRC64NVME |
//!
//! Everything here is pure arithmetic on the base64 values the parts already
//! carry: no object bytes are read at Complete time.

use base64::Engine as _;
use crc::{Algorithm, Crc, CRC_32_ISCSI, CRC_32_ISO_HDLC, CRC_64_NVME};
use sha1::Sha1;
use sha2::{Digest as _, Sha256};

use http::HeaderMap;

use arca_core::types::{MultipartUploadRecord, PartRecord};
use arca_core::s3::xml_types::CompletePart;
use arca_core::{S3Error, S3ErrorCode};

use super::integrity::ChecksumAlgorithm;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// `x-amz-checksum-type`: how the checksum of a multipart object is made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChecksumType {
    Composite,
    FullObject,
}

impl ChecksumType {
    pub fn as_str(self) -> &'static str {
        match self {
            ChecksumType::Composite => "COMPOSITE",
            ChecksumType::FullObject => "FULL_OBJECT",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_uppercase().as_str() {
            "COMPOSITE" => Some(ChecksumType::Composite),
            "FULL_OBJECT" => Some(ChecksumType::FullObject),
            _ => None,
        }
    }

    /// The type S3 picks when the request names none: CRC64NVME only has a
    /// full-object form, every other algorithm defaults to composite.
    pub fn default_for(algorithm: ChecksumAlgorithm) -> Self {
        match algorithm {
            ChecksumAlgorithm::Crc64Nvme => ChecksumType::FullObject,
            _ => ChecksumType::Composite,
        }
    }

    /// Whether `algorithm` can be combined this way: the SHA family has no
    /// full-object form, CRC64NVME no composite one.
    pub fn allows(self, algorithm: ChecksumAlgorithm) -> bool {
        match (self, algorithm) {
            (ChecksumType::Composite, ChecksumAlgorithm::Crc64Nvme) => false,
            (ChecksumType::FullObject, ChecksumAlgorithm::Sha1 | ChecksumAlgorithm::Sha256) => false,
            _ => true,
        }
    }

    /// The type of a stored object checksum: a composite value carries the
    /// `-N` part count, which a base64 string (standard alphabet) never contains.
    pub fn of_value(value: &str) -> Self {
        if value.contains('-') {
            ChecksumType::Composite
        } else {
            ChecksumType::FullObject
        }
    }
}

/// The checksum an upload is created with: `x-amz-checksum-algorithm` and
/// `x-amz-checksum-type` of CreateMultipartUpload. `None` when the request
/// names neither (the upload then has no checksum, as before).
///
/// A type without an algorithm, an algorithm or a type S3 does not have, and a
/// combination that does not exist (SHA with FULL_OBJECT, CRC64NVME with
/// COMPOSITE) are `400 InvalidRequest`. Without a type the algorithm's default
/// is chosen, so the result always carries the type that will be used.
pub fn select_for_upload(
    headers: &HeaderMap,
    resource: &str,
) -> Result<Option<(ChecksumAlgorithm, ChecksumType)>, S3Error> {
    let invalid = |message: String| S3Error::with_message(S3ErrorCode::InvalidRequest, message, resource);
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim);

    let algorithm = match header("x-amz-checksum-algorithm") {
        None => None,
        Some(name) => Some(ChecksumAlgorithm::from_name(name).ok_or_else(|| {
            invalid(
                "Checksum algorithm provided is unsupported. Please try again with any of the valid types: [CRC32, CRC32C, SHA1, SHA256, CRC64NVME]"
                    .to_string(),
            )
        })?),
    };
    let kind = match header("x-amz-checksum-type") {
        None => None,
        Some(value) => Some(
            ChecksumType::parse(value)
                .ok_or_else(|| invalid("Value for x-amz-checksum-type header is invalid.".to_string()))?,
        ),
    };
    match (algorithm, kind) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err(invalid(
            "The x-amz-checksum-type header can only be used with the x-amz-checksum-algorithm header."
                .to_string(),
        )),
        (Some(algorithm), None) => Ok(Some((algorithm, ChecksumType::default_for(algorithm)))),
        (Some(algorithm), Some(kind)) if kind.allows(algorithm) => Ok(Some((algorithm, kind))),
        (Some(algorithm), Some(kind)) => Err(invalid(format!(
            "The {} checksum type can not be used with the {} checksum algorithm.",
            kind.as_str(),
            algorithm.name()
        ))),
    }
}

/// The checksum CompleteMultipartUpload gives the object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectChecksum {
    pub algorithm: ChecksumAlgorithm,
    pub kind: ChecksumType,
    /// Base64; a COMPOSITE value ends with `-<number of parts>`.
    pub value: String,
}

/// Everything CompleteMultipartUpload checks about checksums and sizes, before
/// any blob is assembled, and the checksum the object will get.
///
/// `stored` are the parts the request names, in order, as UploadPart recorded
/// them; `requested` is the same list as the request XML gives it. In order:
///
/// 1. `x-amz-mp-object-size`, when sent, must equal the size of the object
///    (`400 InvalidRequest`).
/// 2. For an upload with a checksum algorithm: `x-amz-checksum-type`, when
///    sent, must be the upload's type (`400 InvalidRequest`).
/// 3. A whole-object `x-amz-checksum-<algorithm>` header must be the checksum
///    computed from the parts (`400 BadDigest`; one of another algorithm is
///    `400 InvalidRequest`). It is checked before the part checksums of the
///    XML, which the Ceph s3-tests pin down.
/// 4. A part checksum given in the XML must equal the one stored for that part
///    (`400 BadDigest`); a part left without one in the XML is accepted.
///
/// The object gets a checksum only when the upload has an algorithm and every
/// part has a stored checksum (a part uploaded by an older version may not).
pub fn plan_complete(
    upload: &MultipartUploadRecord,
    stored: &[&PartRecord],
    requested: &[CompletePart],
    headers: &HeaderMap,
    resource: &str,
) -> Result<Option<ObjectChecksum>, S3Error> {
    let invalid = |message: String| S3Error::with_message(S3ErrorCode::InvalidRequest, message, resource);
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim);

    if let Some(declared) = header("x-amz-mp-object-size") {
        let total: u64 = stored.iter().map(|p| p.size).sum();
        match declared.parse::<u64>() {
            Ok(n) if n == total => {}
            _ => {
                return Err(invalid(format!(
                    "The provided 'x-amz-mp-object-size' header value {declared} does not match what was computed: {total}"
                )))
            }
        }
    }

    let Some(algorithm) = upload.checksum_algorithm.as_deref().and_then(ChecksumAlgorithm::from_name) else {
        return Ok(None);
    };
    let kind = upload_type(algorithm, upload.checksum_type.as_deref());

    if let Some(declared) = header("x-amz-checksum-type") {
        if ChecksumType::parse(declared) != Some(kind) {
            return Err(invalid(format!(
                "The upload was created with the {} checksum type, the complete request says {declared}.",
                kind.as_str()
            )));
        }
    }

    let parts: Vec<PartChecksum<'_>> = stored
        .iter()
        .map(|p| PartChecksum { checksum: p.checksum_value.as_deref(), size: p.size })
        .collect();
    let computed = object_checksum(algorithm, kind, &parts);

    for declared_algorithm in ChecksumAlgorithm::ALL {
        let Some(declared) = header(declared_algorithm.header_name()) else {
            continue;
        };
        if declared_algorithm != algorithm {
            return Err(invalid(format!(
                "Checksum Type mismatch occurred, expected checksum Type: {}, actual checksum Type: {}",
                algorithm.name().to_ascii_lowercase(),
                declared_algorithm.name().to_ascii_lowercase()
            )));
        }
        if let Some(computed) = &computed {
            if declared != computed {
                return Err(S3Error::with_message(
                    S3ErrorCode::BadDigest,
                    format!("The {} you specified did not match the calculated checksum.", algorithm.name()),
                    resource,
                ));
            }
        }
    }

    for (stored, requested) in stored.iter().zip(requested) {
        if let (Some(given), Some(have)) = (requested.checksum_for(algorithm.name()), &stored.checksum_value) {
            if given != have {
                return Err(S3Error::with_message(
                    S3ErrorCode::BadDigest,
                    format!(
                        "The {} you specified for part {} did not match what we received.",
                        algorithm.name(),
                        stored.part_number
                    ),
                    resource,
                ));
            }
        }
    }

    Ok(computed.map(|value| ObjectChecksum { algorithm, kind, value }))
}

/// The type of an upload as recorded: the stored one, or the algorithm's
/// default for an upload created before the type was recorded.
pub fn upload_type(algorithm: ChecksumAlgorithm, stored: Option<&str>) -> ChecksumType {
    stored
        .and_then(ChecksumType::parse)
        .filter(|kind| kind.allows(algorithm))
        .unwrap_or_else(|| ChecksumType::default_for(algorithm))
}

/// `crc1` is the CRC of a block A, `crc2` the CRC of a block B of `len2`
/// bytes; the result is the CRC of A followed by B. Big-endian bytes in and
/// out, as S3 encodes CRCs. `None` for a non-CRC algorithm or a value of the
/// wrong length.
///
/// This is zlib's `crc32_combine` (multiplication by x^(8*len2) in GF(2),
/// done with matrix squaring, so it costs O(log len2)) generalised to the
/// width and polynomial of each algorithm. It holds for the CRCs S3 uses
/// because all three are reflected with an initial value and a final XOR of
/// all ones.
pub fn crc_combine(
    algorithm: ChecksumAlgorithm,
    crc1: &[u8],
    crc2: &[u8],
    len2: u64,
) -> Option<Vec<u8>> {
    match algorithm {
        ChecksumAlgorithm::Crc32 => combine_with(&CRC_32_ISO_HDLC, crc1, crc2, len2),
        ChecksumAlgorithm::Crc32c => combine_with(&CRC_32_ISCSI, crc1, crc2, len2),
        ChecksumAlgorithm::Crc64Nvme => combine_with(&CRC_64_NVME, crc1, crc2, len2),
        ChecksumAlgorithm::Sha1 | ChecksumAlgorithm::Sha256 => None,
    }
}

trait Width: Copy {
    const BYTES: usize;
    fn from_be(bytes: &[u8]) -> Self;
    fn to_be(self) -> Vec<u8>;
    fn widen(self) -> u64;
    fn narrow(value: u64) -> Self;
}

impl Width for u32 {
    const BYTES: usize = 4;
    fn from_be(bytes: &[u8]) -> Self {
        u32::from_be_bytes(bytes.try_into().expect("length checked"))
    }
    fn to_be(self) -> Vec<u8> {
        self.to_be_bytes().to_vec()
    }
    fn widen(self) -> u64 {
        self as u64
    }
    fn narrow(value: u64) -> Self {
        value as u32
    }
}

impl Width for u64 {
    const BYTES: usize = 8;
    fn from_be(bytes: &[u8]) -> Self {
        u64::from_be_bytes(bytes.try_into().expect("length checked"))
    }
    fn to_be(self) -> Vec<u8> {
        self.to_be_bytes().to_vec()
    }
    fn widen(self) -> u64 {
        self
    }
    fn narrow(value: u64) -> Self {
        value
    }
}

fn combine_with<W: Width + crc::Width>(
    algorithm: &Algorithm<W>,
    crc1: &[u8],
    crc2: &[u8],
    len2: u64,
) -> Option<Vec<u8>> {
    if crc1.len() != W::BYTES || crc2.len() != W::BYTES {
        return None;
    }
    let width = algorithm.width as usize;
    let poly: u64 = algorithm.poly.widen();
    // The reflected form of the polynomial: bit-reverse it within `width` bits.
    let reflected = poly.reverse_bits() >> (64 - width);
    let first = W::from_be(crc1).widen();
    let second = W::from_be(crc2).widen();
    Some(W::narrow(combine_raw(first, second, len2, width, reflected)).to_be())
}

fn combine_raw(mut crc1: u64, crc2: u64, mut len2: u64, width: usize, reflected_poly: u64) -> u64 {
    if len2 == 0 {
        return crc1;
    }
    let mut even = vec![0u64; width]; // even-power-of-two zeros operator
    let mut odd = vec![0u64; width]; // odd-power-of-two zeros operator

    // Operator for one zero bit.
    odd[0] = reflected_poly;
    let mut row = 1u64;
    for slot in odd.iter_mut().skip(1) {
        *slot = row;
        row <<= 1;
    }
    // Two zero bits, then four: the first squaring below is for one zero byte.
    square(&mut even, &odd);
    square(&mut odd, &even);

    loop {
        square(&mut even, &odd);
        if len2 & 1 == 1 {
            crc1 = times(&even, crc1);
        }
        len2 >>= 1;
        if len2 == 0 {
            break;
        }
        square(&mut odd, &even);
        if len2 & 1 == 1 {
            crc1 = times(&odd, crc1);
        }
        len2 >>= 1;
        if len2 == 0 {
            break;
        }
    }
    crc1 ^ crc2
}

fn times(matrix: &[u64], mut vector: u64) -> u64 {
    let mut sum = 0;
    let mut row = 0;
    while vector != 0 {
        if vector & 1 == 1 {
            sum ^= matrix[row];
        }
        vector >>= 1;
        row += 1;
    }
    sum
}

fn square(out: &mut [u64], matrix: &[u64]) {
    for n in 0..matrix.len() {
        out[n] = times(matrix, matrix[n]);
    }
}

/// One part as the object checksum is built from it.
#[derive(Debug, Clone, Copy)]
pub struct PartChecksum<'a> {
    /// Base64 checksum of the part; `None` when the part has none.
    pub checksum: Option<&'a str>,
    pub size: u64,
}

/// The checksum of a multipart object, `None` when it cannot be known: a part
/// without a checksum (or with one of the wrong shape), an algorithm the type
/// does not allow, or no parts.
pub fn object_checksum(
    algorithm: ChecksumAlgorithm,
    kind: ChecksumType,
    parts: &[PartChecksum<'_>],
) -> Option<String> {
    if parts.is_empty() || !kind.allows(algorithm) {
        return None;
    }
    let mut raw: Vec<Vec<u8>> = Vec::with_capacity(parts.len());
    for part in parts {
        let bytes = B64.decode(part.checksum?.trim()).ok()?;
        if bytes.len() != algorithm.digest_len() {
            return None;
        }
        raw.push(bytes);
    }
    match kind {
        ChecksumType::Composite => {
            let concatenated: Vec<u8> = raw.concat();
            let digest = match algorithm {
                ChecksumAlgorithm::Sha1 => Sha1::digest(&concatenated).to_vec(),
                ChecksumAlgorithm::Sha256 => Sha256::digest(&concatenated).to_vec(),
                ChecksumAlgorithm::Crc32 => {
                    Crc::<u32, crc::Table<16>>::new(&CRC_32_ISO_HDLC)
                        .checksum(&concatenated)
                        .to_be_bytes()
                        .to_vec()
                }
                ChecksumAlgorithm::Crc32c => {
                    Crc::<u32, crc::Table<16>>::new(&CRC_32_ISCSI)
                        .checksum(&concatenated)
                        .to_be_bytes()
                        .to_vec()
                }
                ChecksumAlgorithm::Crc64Nvme => return None,
            };
            Some(format!("{}-{}", B64.encode(digest), parts.len()))
        }
        ChecksumType::FullObject => {
            let mut combined = raw[0].clone();
            for (next, part) in raw.iter().zip(parts).skip(1) {
                combined = crc_combine(algorithm, &combined, next, part.size)?;
            }
            Some(B64.encode(combined))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crc_of(algorithm: ChecksumAlgorithm, data: &[u8]) -> Vec<u8> {
        match algorithm {
            ChecksumAlgorithm::Crc32 => Crc::<u32, crc::Table<16>>::new(&CRC_32_ISO_HDLC)
                .checksum(data)
                .to_be_bytes()
                .to_vec(),
            ChecksumAlgorithm::Crc32c => Crc::<u32, crc::Table<16>>::new(&CRC_32_ISCSI)
                .checksum(data)
                .to_be_bytes()
                .to_vec(),
            ChecksumAlgorithm::Crc64Nvme => Crc::<u64, crc::Table<16>>::new(&CRC_64_NVME)
                .checksum(data)
                .to_be_bytes()
                .to_vec(),
            _ => unreachable!("not a CRC"),
        }
    }

    const CRCS: [ChecksumAlgorithm; 3] = [
        ChecksumAlgorithm::Crc32,
        ChecksumAlgorithm::Crc32c,
        ChecksumAlgorithm::Crc64Nvme,
    ];

    /// Deterministic pseudo-random bytes (no dependency on a RNG crate).
    fn bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect()
    }

    #[test]
    fn combine_equals_the_crc_of_the_concatenation() {
        for algorithm in CRCS {
            for (seed, (la, lb)) in [(0, 0), (0, 1), (1, 0), (1, 1), (3, 5), (64, 64), (1000, 1), (1, 1000), (4096, 12345), (70000, 33333)]
                .into_iter()
                .enumerate()
            {
                let a = bytes(seed as u64, la);
                let b = bytes(seed as u64 + 99, lb);
                let whole = [a.clone(), b.clone()].concat();
                let combined =
                    crc_combine(algorithm, &crc_of(algorithm, &a), &crc_of(algorithm, &b), lb as u64).unwrap();
                assert_eq!(combined, crc_of(algorithm, &whole), "{algorithm:?} {la}+{lb}");
            }
        }
    }

    #[test]
    fn combine_folds_over_many_parts() {
        for algorithm in CRCS {
            let parts: Vec<Vec<u8>> = (0..7).map(|i| bytes(i, 1000 + 317 * i as usize)).collect();
            let whole: Vec<u8> = parts.concat();
            let mut acc = crc_of(algorithm, &parts[0]);
            for part in &parts[1..] {
                acc = crc_combine(algorithm, &acc, &crc_of(algorithm, part), part.len() as u64).unwrap();
            }
            assert_eq!(acc, crc_of(algorithm, &whole), "{algorithm:?}");
        }
    }

    #[test]
    fn combine_refuses_non_crcs_and_wrong_lengths() {
        assert!(crc_combine(ChecksumAlgorithm::Sha256, &[0; 32], &[0; 32], 1).is_none());
        assert!(crc_combine(ChecksumAlgorithm::Sha1, &[0; 20], &[0; 20], 1).is_none());
        assert!(crc_combine(ChecksumAlgorithm::Crc32, &[0; 4], &[0; 8], 1).is_none());
        assert!(crc_combine(ChecksumAlgorithm::Crc64Nvme, &[0; 4], &[0; 4], 1).is_none());
    }

    // The vectors of ceph/s3-tests (test_multipart_use_cksum_helper_*, computed
    // there by `unittest_rgw_cksum`): three parts of 5 MiB made of `A`, `B`, `C`.
    const SIZE: u64 = 5 * 1024 * 1024;

    fn ceph_part(byte: u8) -> Vec<u8> {
        vec![byte; SIZE as usize]
    }

    fn b64(raw: &[u8]) -> String {
        B64.encode(raw)
    }

    fn parts<'a>(values: &'a [&'a str]) -> Vec<PartChecksum<'a>> {
        values
            .iter()
            .map(|v| PartChecksum { checksum: Some(v), size: SIZE })
            .collect()
    }

    #[test]
    fn ceph_vectors_full_object_crcs() {
        let cases: [(ChecksumAlgorithm, [&str; 3], &str); 3] = [
            (ChecksumAlgorithm::Crc32, ["JRTCyQ==", "QoZTGg==", "YAgjqw=="], "WgDhBQ=="),
            (ChecksumAlgorithm::Crc32c, ["MDaLrw==", "TH4EZg==", "Z7mBIQ=="], "xU+Krw=="),
            (ChecksumAlgorithm::Crc64Nvme, ["L/E4WYn8v98=", "xW1l19VobYM=", "cK5MnNaWrW4="], "i+6LR0y3eFo="),
        ];
        for (algorithm, part_values, full) in cases {
            // The part values really are the checksums of those bytes...
            for (byte, expected) in [b'A', b'B', b'C'].into_iter().zip(part_values) {
                assert_eq!(b64(&crc_of(algorithm, &ceph_part(byte))), expected, "{algorithm:?} {byte}");
            }
            // ...and combining them gives the checksum Ceph computed for the whole object.
            let got = object_checksum(algorithm, ChecksumType::FullObject, &parts(&part_values));
            assert_eq!(got.as_deref(), Some(full), "{algorithm:?}");
            // The whole object, hashed in one go, agrees.
            let whole = [ceph_part(b'A'), ceph_part(b'B'), ceph_part(b'C')].concat();
            assert_eq!(b64(&crc_of(algorithm, &whole)), full, "{algorithm:?} whole");
        }
    }

    #[test]
    fn ceph_vectors_composite_sha() {
        let sha256 = ["275VF5loJr1YYawit0XSHREhkFXYkkPKGuoK0x9VKxI=", "mrHwOfjTL5Zwfj74F05HOQGLdUb7E5szdCbxgUSq6NM=", "Vw7oB/nKQ5xWb3hNgbyfkvDiivl+U+/Dft48nfJfDow="];
        assert_eq!(
            object_checksum(ChecksumAlgorithm::Sha256, ChecksumType::Composite, &parts(&sha256)).as_deref(),
            Some("uWBwpe1dxI4Vw8Gf0X9ynOdw/SS6VBzfWm9giiv1sf4=-3")
        );
        let sha1 = ["iIaTCGbm+vdVjNqIMF2S0T7ibMk=", "LS/TJ32bAVKEwRu+sE3X7awh/lk=", "6DDwovUaHwrKNXDMzOGbuvj9kxI="];
        assert_eq!(
            object_checksum(ChecksumAlgorithm::Sha1, ChecksumType::Composite, &parts(&sha1)).as_deref(),
            Some("sizjvY4eud3MrcHdZM3cQ/ol39o=-3")
        );
        // A single part of the test_multipart_checksum_sha256 test (1 KiB of `A`).
        let one = [PartChecksum { checksum: Some("arcu6553sHVAiX4MjW0j7I7vD4w6R+Gz9Ok0Q9lTa+0="), size: 1024 }];
        assert_eq!(
            object_checksum(ChecksumAlgorithm::Sha256, ChecksumType::Composite, &one).as_deref(),
            Some("Ok6Cs5b96ux6+MWQkJO7UBT5sKPBeXBLwvj/hK89smg=-1")
        );
    }

    #[test]
    fn composite_crc_is_the_crc_of_the_part_crcs() {
        // Defined by S3: the same algorithm over the concatenated raw part checksums.
        for algorithm in [ChecksumAlgorithm::Crc32, ChecksumAlgorithm::Crc32c] {
            let a = bytes(1, 100);
            let b = bytes(2, 200);
            let (ca, cb) = (crc_of(algorithm, &a), crc_of(algorithm, &b));
            let expected = format!("{}-2", b64(&crc_of(algorithm, &[ca.clone(), cb.clone()].concat())));
            let (sa, sb) = (b64(&ca), b64(&cb));
            let got = object_checksum(
                algorithm,
                ChecksumType::Composite,
                &[PartChecksum { checksum: Some(&sa), size: 100 }, PartChecksum { checksum: Some(&sb), size: 200 }],
            );
            assert_eq!(got.as_deref(), Some(expected.as_str()), "{algorithm:?}");
        }
    }

    #[test]
    fn a_part_without_a_usable_checksum_gives_no_object_checksum() {
        let ok = "arcu6553sHVAiX4MjW0j7I7vD4w6R+Gz9Ok0Q9lTa+0=";
        let missing = [
            PartChecksum { checksum: Some(ok), size: 1 },
            PartChecksum { checksum: None, size: 1 },
        ];
        assert!(object_checksum(ChecksumAlgorithm::Sha256, ChecksumType::Composite, &missing).is_none());
        let not_base64 = [PartChecksum { checksum: Some("%%%"), size: 1 }];
        assert!(object_checksum(ChecksumAlgorithm::Sha256, ChecksumType::Composite, &not_base64).is_none());
        let wrong_length = [PartChecksum { checksum: Some("JRTCyQ=="), size: 1 }];
        assert!(object_checksum(ChecksumAlgorithm::Sha256, ChecksumType::Composite, &wrong_length).is_none());
        assert!(object_checksum(ChecksumAlgorithm::Sha256, ChecksumType::Composite, &[]).is_none());
    }

    #[test]
    fn type_and_algorithm_combinations() {
        use ChecksumAlgorithm::*;
        use ChecksumType::*;
        for (algorithm, composite, full) in [
            (Crc32, true, true),
            (Crc32c, true, true),
            (Crc64Nvme, false, true),
            (Sha1, true, false),
            (Sha256, true, false),
        ] {
            assert_eq!(Composite.allows(algorithm), composite, "{algorithm:?} composite");
            assert_eq!(FullObject.allows(algorithm), full, "{algorithm:?} full");
        }
        assert_eq!(ChecksumType::default_for(Crc64Nvme), FullObject);
        for algorithm in [Crc32, Crc32c, Sha1, Sha256] {
            assert_eq!(ChecksumType::default_for(algorithm), Composite);
        }
        // Everything a default can pick is allowed.
        for algorithm in ChecksumAlgorithm::ALL {
            assert!(ChecksumType::default_for(algorithm).allows(algorithm));
        }
    }

    fn selection(pairs: &[(&str, &str)]) -> Result<Option<(ChecksumAlgorithm, ChecksumType)>, S3Error> {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(http::HeaderName::from_bytes(name.as_bytes()).unwrap(), value.parse().unwrap());
        }
        select_for_upload(&headers, "/b/k")
    }

    #[test]
    fn an_upload_without_checksum_headers_has_no_checksum() {
        assert_eq!(selection(&[]).unwrap(), None);
    }

    #[test]
    fn the_algorithm_alone_gets_its_default_type() {
        use ChecksumAlgorithm::*;
        for (name, algorithm, kind) in [
            ("CRC32", Crc32, ChecksumType::Composite),
            ("crc32c", Crc32c, ChecksumType::Composite),
            ("SHA1", Sha1, ChecksumType::Composite),
            ("sha256", Sha256, ChecksumType::Composite),
            ("CRC64NVME", Crc64Nvme, ChecksumType::FullObject),
        ] {
            assert_eq!(
                selection(&[("x-amz-checksum-algorithm", name)]).unwrap(),
                Some((algorithm, kind)),
                "{name}"
            );
        }
    }

    #[test]
    fn an_explicit_type_is_kept_when_the_algorithm_allows_it() {
        assert_eq!(
            selection(&[("x-amz-checksum-algorithm", "CRC32"), ("x-amz-checksum-type", "full_object")]).unwrap(),
            Some((ChecksumAlgorithm::Crc32, ChecksumType::FullObject))
        );
        assert_eq!(
            selection(&[("x-amz-checksum-algorithm", "CRC32C"), ("x-amz-checksum-type", "COMPOSITE")]).unwrap(),
            Some((ChecksumAlgorithm::Crc32c, ChecksumType::Composite))
        );
    }

    #[test]
    fn contradictory_or_unknown_selections_are_invalid_requests() {
        for pairs in [
            vec![("x-amz-checksum-algorithm", "SHA256"), ("x-amz-checksum-type", "FULL_OBJECT")],
            vec![("x-amz-checksum-algorithm", "SHA1"), ("x-amz-checksum-type", "FULL_OBJECT")],
            vec![("x-amz-checksum-algorithm", "CRC64NVME"), ("x-amz-checksum-type", "COMPOSITE")],
            vec![("x-amz-checksum-type", "COMPOSITE")],
            vec![("x-amz-checksum-algorithm", "MD5")],
            vec![("x-amz-checksum-algorithm", "CRC32"), ("x-amz-checksum-type", "WHOLE")],
        ] {
            let err = selection(&pairs).unwrap_err();
            assert_eq!(err.code, S3ErrorCode::InvalidRequest, "{pairs:?}");
        }
    }

    #[test]
    fn the_type_of_an_upload_falls_back_to_the_default() {
        use ChecksumAlgorithm::*;
        assert_eq!(upload_type(Crc32, Some("FULL_OBJECT")), ChecksumType::FullObject);
        assert_eq!(upload_type(Crc32, None), ChecksumType::Composite);
        assert_eq!(upload_type(Crc64Nvme, None), ChecksumType::FullObject);
        // A recorded type the algorithm cannot have is ignored rather than trusted.
        assert_eq!(upload_type(Sha256, Some("FULL_OBJECT")), ChecksumType::Composite);
        assert_eq!(upload_type(Crc32, Some("garbage")), ChecksumType::Composite);
    }

    #[test]
    fn the_type_of_a_stored_value_comes_from_its_suffix() {
        assert_eq!(ChecksumType::of_value("uWBwpe1dxI4Vw8Gf0X9ynOdw/SS6VBzfWm9giiv1sf4=-3"), ChecksumType::Composite);
        assert_eq!(ChecksumType::of_value("WgDhBQ=="), ChecksumType::FullObject);
        assert_eq!(ChecksumType::parse("composite"), Some(ChecksumType::Composite));
        assert_eq!(ChecksumType::parse("FULL_OBJECT"), Some(ChecksumType::FullObject));
        assert_eq!(ChecksumType::parse("other"), None);
    }

    // -- plan_complete --

    use arca_core::types::BlobId;

    fn upload(algorithm: Option<&str>, kind: Option<&str>) -> MultipartUploadRecord {
        MultipartUploadRecord {
            upload_id: "u".into(),
            bucket: "b".into(),
            key: "k".into(),
            content_type: None,
            initiated_at: chrono::Utc::now(),
            metadata: Default::default(),
            checksum_algorithm: algorithm.map(str::to_string),
            checksum_type: kind.map(str::to_string),
            initiator: "root".into(),
        }
    }

    fn stored(number: u32, size: u64, checksum: Option<&str>) -> PartRecord {
        PartRecord {
            upload_id: "u".into(),
            part_number: number,
            blob_id: BlobId::new(),
            size,
            etag: "e".into(),
            checksum_value: checksum.map(str::to_string),
            last_modified: None,
        }
    }

    fn requested(number: u32, sha256: Option<&str>) -> CompletePart {
        CompletePart {
            part_number: number,
            etag: "e".into(),
            checksum_crc32: None,
            checksum_crc32c: None,
            checksum_crc64nvme: None,
            checksum_sha1: None,
            checksum_sha256: sha256.map(str::to_string),
        }
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(http::HeaderName::from_bytes(name.as_bytes()).unwrap(), value.parse().unwrap());
        }
        map
    }

    const P1: &str = "275VF5loJr1YYawit0XSHREhkFXYkkPKGuoK0x9VKxI=";
    const P2: &str = "mrHwOfjTL5Zwfj74F05HOQGLdUb7E5szdCbxgUSq6NM=";
    const COMPOSITE_OF_P1_P2_P3: &str = "uWBwpe1dxI4Vw8Gf0X9ynOdw/SS6VBzfWm9giiv1sf4=-3";

    fn plan(
        up: &MultipartUploadRecord,
        parts: &[PartRecord],
        req: &[CompletePart],
        h: &HeaderMap,
    ) -> Result<Option<ObjectChecksum>, S3Error> {
        let refs: Vec<&PartRecord> = parts.iter().collect();
        plan_complete(up, &refs, req, h, "/b/k")
    }

    #[test]
    fn an_upload_without_an_algorithm_gets_no_checksum_whatever_is_sent() {
        let parts = [stored(1, 5, Some(P1))];
        let got = plan(&upload(None, None), &parts, &[requested(1, Some(P1))], &headers(&[("x-amz-checksum-sha256", "bad")]));
        assert_eq!(got.unwrap(), None);
    }

    #[test]
    fn the_checksum_is_built_from_the_stored_part_checksums() {
        let parts = [stored(1, 5 * 1024 * 1024, Some(P1)), stored(2, 5, Some(P2))];
        let got = plan(&upload(Some("SHA256"), Some("COMPOSITE")), &parts, &[], &headers(&[])).unwrap().unwrap();
        assert_eq!(got.algorithm, ChecksumAlgorithm::Sha256);
        assert_eq!(got.kind, ChecksumType::Composite);
        assert!(got.value.ends_with("-2"));
        // The recorded type is what counts; an upload without one uses the default.
        let same = plan(&upload(Some("SHA256"), None), &parts, &[], &headers(&[])).unwrap().unwrap();
        assert_eq!(same, got);
    }

    #[test]
    fn a_part_without_a_stored_checksum_means_no_object_checksum_and_no_error() {
        let parts = [stored(1, 5, Some(P1)), stored(2, 5, None)];
        assert_eq!(plan(&upload(Some("SHA256"), None), &parts, &[], &headers(&[])).unwrap(), None);
        // The request header cannot be checked then, and is accepted.
        let h = headers(&[("x-amz-checksum-sha256", "whatever")]);
        assert_eq!(plan(&upload(Some("SHA256"), None), &parts, &[], &h).unwrap(), None);
    }

    #[test]
    fn a_wrong_object_checksum_header_is_bad_digest() {
        let parts = [stored(1, 5, Some(P1)), stored(2, 5, Some(P2))];
        let up = upload(Some("SHA256"), None);
        let err = plan(&up, &parts, &[], &headers(&[("x-amz-checksum-sha256", "bad")])).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::BadDigest);
        // The value the server would compute is accepted.
        let value = plan(&up, &parts, &[], &headers(&[])).unwrap().unwrap().value;
        assert!(plan(&up, &parts, &[], &headers(&[("x-amz-checksum-sha256", &value)])).is_ok());
    }

    #[test]
    fn the_object_checksum_header_is_checked_before_the_part_checksums() {
        // Ceph: a bad header with a part checksum missing from the XML is BadDigest.
        let parts = [stored(1, 5, Some(P1))];
        let err = plan(&upload(Some("SHA256"), None), &parts, &[requested(1, None)], &headers(&[("x-amz-checksum-sha256", "bad")])).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::BadDigest);
        assert!(err.message.contains("did not match the calculated checksum"), "{}", err.message);
    }

    #[test]
    fn a_header_of_another_algorithm_is_an_invalid_request() {
        let parts = [stored(1, 5, Some(P1))];
        let err = plan(&upload(Some("SHA256"), None), &parts, &[], &headers(&[("x-amz-checksum-crc32", "AAAAAA==")])).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidRequest);
        assert!(err.message.contains("expected checksum Type: sha256, actual checksum Type: crc32"), "{}", err.message);
    }

    #[test]
    fn a_part_checksum_in_the_xml_must_match_and_may_be_left_out() {
        let parts = [stored(1, 5, Some(P1)), stored(2, 5, Some(P2))];
        let up = upload(Some("SHA256"), None);
        let err = plan(&up, &parts, &[requested(1, Some(P1)), requested(2, Some(P1))], &headers(&[])).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::BadDigest);
        assert!(err.message.contains("part 2"), "{}", err.message);
        assert!(plan(&up, &parts, &[requested(1, Some(P1)), requested(2, Some(P2))], &headers(&[])).is_ok());
        assert!(plan(&up, &parts, &[requested(1, None), requested(2, None)], &headers(&[])).is_ok());
    }

    #[test]
    fn the_checksum_type_header_must_be_the_uploads() {
        let parts = [stored(1, 5, Some("JRTCyQ=="))];
        let up = upload(Some("CRC32"), Some("FULL_OBJECT"));
        let err = plan(&up, &parts, &[], &headers(&[("x-amz-checksum-type", "COMPOSITE")])).unwrap_err();
        assert_eq!(err.code, S3ErrorCode::InvalidRequest);
        assert!(plan(&up, &parts, &[], &headers(&[("x-amz-checksum-type", "full_object")])).is_ok());
    }

    #[test]
    fn the_object_size_header_must_be_the_assembled_size() {
        let parts = [stored(1, 5, None), stored(2, 7, None)];
        let up = upload(None, None);
        assert!(plan(&up, &parts, &[], &headers(&[("x-amz-mp-object-size", "12")])).is_ok());
        for wrong in ["11", "13", "abc", "-12"] {
            let err = plan(&up, &parts, &[], &headers(&[("x-amz-mp-object-size", wrong)])).unwrap_err();
            assert_eq!(err.code, S3ErrorCode::InvalidRequest, "{wrong}");
        }
    }

    #[test]
    fn a_full_object_header_is_the_crc_of_the_whole_object() {
        // The Ceph CRC32 vector: parts of 5 MiB of A, B, C.
        let parts = [
            stored(1, SIZE, Some("JRTCyQ==")),
            stored(2, SIZE, Some("QoZTGg==")),
            stored(3, SIZE, Some("YAgjqw==")),
        ];
        let up = upload(Some("CRC32"), Some("FULL_OBJECT"));
        let h = headers(&[("x-amz-checksum-crc32", "WgDhBQ=="), ("x-amz-checksum-type", "FULL_OBJECT")]);
        let got = plan(&up, &parts, &[], &h).unwrap().unwrap();
        assert_eq!(got.value, "WgDhBQ==");
        assert_eq!(got.kind, ChecksumType::FullObject);
        let wrong = headers(&[("x-amz-checksum-crc32", "AAAAAA==")]);
        assert_eq!(plan(&up, &parts, &[], &wrong).unwrap_err().code, S3ErrorCode::BadDigest);
    }

    #[test]
    fn the_ceph_composite_vector_through_plan_complete() {
        // test_multipart_use_cksum_helper_sha256: three parts of 5 MiB of A, B, C.
        let parts = [
            stored(1, SIZE, Some(P1)),
            stored(2, SIZE, Some(P2)),
            stored(3, SIZE, Some("Vw7oB/nKQ5xWb3hNgbyfkvDiivl+U+/Dft48nfJfDow=")),
        ];
        let h = headers(&[("x-amz-checksum-sha256", COMPOSITE_OF_P1_P2_P3)]);
        let got = plan(&upload(Some("SHA256"), Some("COMPOSITE")), &parts, &[], &h).unwrap().unwrap();
        assert_eq!(got.value, COMPOSITE_OF_P1_P2_P3);
    }
}
