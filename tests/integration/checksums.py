"""Reference checksums for the integration tests, computed in plain Python.

botocore only computes CRC32C and CRC64NVME when `awscrt` is installed, which
the test image does not have, so the suites compute them here (table driven,
about a second per 5 MiB), independently of the server's implementation.
"""

import base64
import functools
import hashlib
import zlib


def _table(poly):
    table = []
    for n in range(256):
        crc = n
        for _ in range(8):
            crc = (crc >> 1) ^ (poly if crc & 1 else 0)
        table.append(crc)
    return table


_TABLES = {
    0x82F63B78: _table(0x82F63B78),
    0x9A6C9329AC4BC9B5: _table(0x9A6C9329AC4BC9B5),
}


def _crc_reflected(data, poly, init, xorout):
    table = _TABLES[poly]
    crc = init
    for byte in data:
        crc = (crc >> 8) ^ table[(crc ^ byte) & 0xFF]
    return crc ^ xorout


def crc32c(data):
    return _crc_reflected(data, 0x82F63B78, 0xFFFFFFFF, 0xFFFFFFFF)


def crc64nvme(data):
    return _crc_reflected(
        data, 0x9A6C9329AC4BC9B5, 0xFFFFFFFFFFFFFFFF, 0xFFFFFFFFFFFFFFFF,
    )


def b64(raw):
    return base64.b64encode(raw).decode()


def raw(value):
    return base64.b64decode(value)


# algorithm name -> raw digest of `data` (CRCs big-endian, as S3 encodes them)
RAW = {
    "CRC32": lambda d: zlib.crc32(d).to_bytes(4, "big"),
    "CRC32C": lambda d: crc32c(d).to_bytes(4, "big"),
    "CRC64NVME": lambda d: crc64nvme(d).to_bytes(8, "big"),
    "SHA1": lambda d: hashlib.sha1(d).digest(),
    "SHA256": lambda d: hashlib.sha256(d).digest(),
}

# algorithm name -> base64 checksum of `data`
CHECKSUMS = {name: (lambda f: lambda d: b64(f(d)))(fn) for name, fn in RAW.items()}


@functools.lru_cache(maxsize=None)
def cached(algorithm, data):
    """`CHECKSUMS[algorithm](data)`, memoised (the payloads are big and reused)."""
    return CHECKSUMS[algorithm](data)


def composite(algorithm, part_checksums):
    """The COMPOSITE checksum S3 gives a multipart object: the algorithm over
    the concatenated raw part checksums, with `-<number of parts>` appended."""
    joined = b"".join(raw(c) for c in part_checksums)
    return f"{b64(RAW[algorithm](joined))}-{len(part_checksums)}"


def md5_b64(data):
    return b64(hashlib.md5(data).digest())
