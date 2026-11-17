#!/usr/bin/env python3
"""Parse and verify the TLS ClientHello of the opencode free transport.

  zen_hello_parse.py [--sni opencode.ai] FILE

Read a raw TLS record (tests/zen_hello_test.c writes one) or ASCII containing
a `*_clienthello_hex=...` / `*hello_hex=...` line. Print a human summary and
one stable `HELLO_CHECKS: {json}` line. Exit 0 only when every required field
matches the captured Bun 1.4.4 fetch hello.

No third-party dependencies, no live network.
"""

import argparse
import json
import re
import struct
import sys

# Exact Bun 1.4.4 fetch() hello, captured from opencode.ai.
EXPECTED_CIPHERS = [
    0x1301, 0x1302, 0x1303,
    0xC02B, 0xC02F, 0xC02C, 0xC030, 0xCCA9, 0xCCA8,
    0xC009, 0xC013, 0xC00A, 0xC014, 0x009C, 0x009D, 0x002F, 0x0035,
]
EXPECTED_EXTENSIONS = [
    0x0000, 0x0017, 0xFF01, 0x000A, 0x000B, 0x0023, 0x0010,
    0x0005, 0x000D, 0x0012, 0x0033, 0x002D, 0x002B,
]
EXPECTED_GROUPS = [0x11EC, 0x001D, 0x0017, 0x0018]
EXPECTED_SIGALGS = [
    0x0403, 0x0804, 0x0401, 0x0503, 0x0805, 0x0501,
    0x0806, 0x0601, 0x0807, 0x0603, 0x0201,
]
EXPECTED_VERSIONS = [0x0304, 0x0303]
EXPECTED_KEY_SHARES = [(0x11EC, 1216), (0x001D, 32)]
EXPECTED_ALPN = ["http/1.1"]
EXPECTED_RECORD_VERSION = 0x0301
EXPECTED_LEGACY_VERSION = 0x0303
EXPECTED_SESSION_ID_LEN = 32
EXPECTED_COMPRESSION = [0]
EXPECTED_PSK_MODES = [1]


def hx(value):
    return "0x%04x" % value


def decode_hex_source(path):
    """Return (bytes, record_version_hint) from a raw capture or a hex dump.

    The spec output of the uTLS helper prints the bare handshake message
    (`generated_clienthello_hex`), which has no TLS record header at all; the
    record version then comes from the helper's own diagnostic line, if any.
    """
    data = open(path, "rb").read()
    if len(data) >= 3 and data[0] == 0x16 and data[1] == 0x03:
        return data, None
    if len(data) >= 4 and data[0] == 1:
        length = int.from_bytes(data[1:4], "big")
        if 4 + length <= len(data):
            return data, None  # bare handshake message, no record header
    text = data.decode("latin1")
    hint = None
    match = re.search(r"record\s+type=\d+\s+version=0x([0-9a-fA-F]{4})", text)
    if not match:
        match = re.search(r"record_version\s*[:=]\s*0x([0-9a-fA-F]{4})", text)
    if match:
        hint = int(match.group(1), 16)
    candidates = []
    for match in re.finditer(
        r"(?im)^[ \t]*[A-Za-z0-9_.-]*hello[A-Za-z0-9_.-]*[ \t]*[:=][ \t]*([0-9a-fA-F]+)[ \t]*\r?$",
        text,
    ):
        candidates.append(match.group(1))
    if not candidates:
        candidates = re.findall(r"[0-9a-fA-F]{80,}", text)
    candidates = [c for c in candidates if len(c) % 2 == 0]
    if not candidates:
        raise ValueError("no ClientHello hex found in %s" % path)
    return bytes.fromhex(max(candidates, key=len)), hint


def split_handshake(data):
    """Reassemble handshake messages from TLS records.

    A ClientHello may span records; concatenate every handshake record payload
    before walking the handshake header. Returns (record_version, messages).
    The helper's spec output is a bare handshake message, so a capture that
    starts with handshake type 1 is accepted with record_version None.
    """
    if data and data[0] == 1:
        length = int.from_bytes(data[1:4], "big")
        if 4 + length <= len(data):
            return None, [(1, data[4:4 + length])]
    record_version = None
    handshake = b""
    offset = 0
    while offset + 5 <= len(data):
        content_type = data[offset]
        version = struct.unpack("!H", data[offset + 1:offset + 3])[0]
        length = struct.unpack("!H", data[offset + 3:offset + 5])[0]
        if offset + 5 + length > len(data):
            break
        payload = data[offset + 5:offset + 5 + length]
        if record_version is None:
            record_version = version
        if content_type == 22:  # handshake
            handshake += payload
        offset += 5 + length
    messages = []
    offset = 0
    while offset + 4 <= len(handshake):
        kind = handshake[offset]
        length = int.from_bytes(handshake[offset + 1:offset + 4], "big")
        if offset + 4 + length > len(handshake):
            break
        messages.append((kind, handshake[offset + 4:offset + 4 + length]))
        offset += 4 + length
    return record_version, messages


def parse_sni(data):
    if len(data) < 2:
        return None
    end = 2 + struct.unpack("!H", data[:2])[0]
    pos = 2
    while pos + 3 <= min(end, len(data)):
        name_type = data[pos]
        name_len = struct.unpack("!H", data[pos + 1:pos + 3])[0]
        pos += 3
        name = data[pos:pos + name_len].decode("latin1")
        pos += name_len
        if name_type == 0:
            return name
    return None


def parse_alpn(data):
    if len(data) < 2:
        return []
    end = 2 + struct.unpack("!H", data[:2])[0]
    pos = 2
    out = []
    while pos < min(end, len(data)):
        length = data[pos]
        pos += 1
        out.append(data[pos:pos + length].decode("latin1"))
        pos += length
    return out


def parse_u16_vector(data):
    if len(data) < 2:
        return []
    count = struct.unpack("!H", data[:2])[0]
    end = 2 + count
    return [
        struct.unpack("!H", data[i:i + 2])[0]
        for i in range(2, min(end, len(data)) - 1, 2)
    ]


def parse_u16_vector8(data):
    """A u16 vector whose length prefix is one byte (supported_versions)."""
    if len(data) < 1:
        return []
    end = 1 + data[0]
    return [
        struct.unpack("!H", data[i:i + 2])[0]
        for i in range(1, min(end, len(data)) - 1, 2)
    ]


def parse_key_shares(data):
    if len(data) < 2:
        return []
    end = 2 + struct.unpack("!H", data[:2])[0]
    pos = 2
    out = []
    while pos + 4 <= min(end, len(data)):
        group = struct.unpack("!H", data[pos:pos + 2])[0]
        length = struct.unpack("!H", data[pos + 2:pos + 4])[0]
        pos += 4
        out.append((group, length))
        pos += length
    return out


def parse_client_hello(body):
    pos = 0
    legacy_version = struct.unpack("!H", body[pos:pos + 2])[0]
    pos += 2
    pos += 32  # random
    session_id_len = body[pos]
    pos += 1
    session_id = body[pos:pos + session_id_len]
    pos += session_id_len
    cipher_len = struct.unpack("!H", body[pos:pos + 2])[0]
    pos += 2
    ciphers = [
        struct.unpack("!H", body[i:i + 2])[0]
        for i in range(pos, pos + cipher_len, 2)
    ]
    pos += cipher_len
    compression_len = body[pos]
    pos += 1
    compression = list(body[pos:pos + compression_len])
    pos += compression_len
    extensions = []
    ext_len = struct.unpack("!H", body[pos:pos + 2])[0]
    pos += 2
    end = pos + ext_len
    while pos + 4 <= end:
        ext_type = struct.unpack("!H", body[pos:pos + 2])[0]
        ext_size = struct.unpack("!H", body[pos + 2:pos + 4])[0]
        pos += 4
        extensions.append((ext_type, body[pos:pos + ext_size]))
        pos += ext_size
    return {
        "legacy_version": legacy_version,
        "session_id_len": session_id_len,
        "session_id": session_id,
        "ciphers": ciphers,
        "compression": compression,
        "extensions": extensions,
    }


def parse_hello(data, record_version_hint=None):
    record_version, messages = split_handshake(data)
    if record_version is None:
        record_version = record_version_hint
    hello = next((body for kind, body in messages if kind == 1), None)
    if hello is None:
        raise ValueError("no ClientHello handshake message in capture")
    parsed = parse_client_hello(hello)
    parsed["record_version"] = record_version
    ext = dict(parsed["extensions"])
    parsed["sni"] = parse_sni(ext.get(0x0000, b""))
    parsed["groups"] = parse_u16_vector(ext.get(0x000A, b""))
    parsed["sigalgs"] = parse_u16_vector(ext.get(0x000D, b""))
    parsed["alpn"] = parse_alpn(ext.get(0x0010, b""))
    parsed["supported_versions"] = parse_u16_vector8(ext.get(0x002B, b""))
    psk = ext.get(0x002D, b"")
    parsed["psk_modes"] = list(psk[1:1 + psk[0]]) if psk else []
    parsed["key_shares"] = parse_key_shares(ext.get(0x0033, b""))
    return parsed


def compare(parsed, expected_sni):
    ciphers = parsed["ciphers"]
    extensions = [kind for kind, _ in parsed["extensions"]]
    compression = parsed["compression"]
    checks = {
        "record_version_0301": parsed["record_version"] == EXPECTED_RECORD_VERSION,
        "legacy_version_0303": parsed["legacy_version"] == EXPECTED_LEGACY_VERSION,
        "session_id_len_32": parsed["session_id_len"] == EXPECTED_SESSION_ID_LEN,
        "compression_null": compression == EXPECTED_COMPRESSION,
        "cipher_count_17": len(ciphers) == len(EXPECTED_CIPHERS),
        "cipher_order": ciphers == EXPECTED_CIPHERS,
        "extension_count_13": len(extensions) == len(EXPECTED_EXTENSIONS),
        "extension_order": extensions == EXPECTED_EXTENSIONS,
        "alpn_http11": parsed["alpn"] == EXPECTED_ALPN,
        "sni": parsed["sni"] == expected_sni,
        "groups": parsed["groups"] == EXPECTED_GROUPS,
        "key_share_lengths": parsed["key_shares"] == EXPECTED_KEY_SHARES,
        "sigalgs": parsed["sigalgs"] == EXPECTED_SIGALGS,
        "sigalgs_include_0807_0603": 0x0807 in parsed["sigalgs"]
        and 0x0603 in parsed["sigalgs"],
        "supported_versions": parsed["supported_versions"] == EXPECTED_VERSIONS,
        "psk_modes_dhe_ke": parsed["psk_modes"] == EXPECTED_PSK_MODES,
    }
    return checks


def summary(parsed):
    print(
        "record_version=%s"
        % ("unknown" if parsed["record_version"] is None else hx(parsed["record_version"]))
    )
    print("legacy_version=%s" % hx(parsed["legacy_version"]))
    print("session_id_len=%d" % parsed["session_id_len"])
    print("cipher_count=%d" % len(parsed["ciphers"]))
    print("ciphers=" + ",".join(hx(c) for c in parsed["ciphers"]))
    print("compression=" + ",".join("%02x" % c for c in parsed["compression"]))
    print("extension_count=%d" % len(parsed["extensions"]))
    print(
        "extension_order="
        + ",".join(hx(kind) for kind, _ in parsed["extensions"])
    )
    print("supported_groups=" + ",".join(hx(g) for g in parsed["groups"]))
    print(
        "key_shares="
        + ",".join("%s:%dB" % (hx(g), length) for g, length in parsed["key_shares"])
    )
    print("sig_algs=" + ",".join(hx(s) for s in parsed["sigalgs"]))
    print("alpn=" + ",".join(parsed["alpn"]))
    print("sni=%s" % parsed["sni"])
    print(
        "supported_versions="
        + ",".join(hx(v) for v in parsed["supported_versions"])
    )
    print("psk_modes=" + ",".join("%02x" % mode for mode in parsed["psk_modes"]))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", nargs="?", help="ClientHello capture or hex output")
    parser.add_argument("--hex", help="ClientHello as a hex string")
    parser.add_argument("--sni", default="opencode.ai", help="expected SNI")
    parser.add_argument("--json", action="store_true", help="print only the JSON line")
    args = parser.parse_args()

    try:
        if args.hex:
            data, hint = bytes.fromhex(re.sub(r"\s+", "", args.hex)), None
        else:
            data, hint = decode_hex_source(args.path)
        parsed = parse_hello(data, hint)
    except (OSError, ValueError, struct.error, IndexError) as error:
        print("HELLO_ERROR: %s" % error, file=sys.stderr)
        return 2

    checks = compare(parsed, args.sni)
    passed = all(checks.values())
    if not args.json:
        summary(parsed)
    print(
        "HELLO_CHECKS: "
        + json.dumps({"checks": checks, "pass": passed}, sort_keys=True),
        flush=True,
    )
    return 0 if passed else 1


if __name__ == "__main__":
    sys.exit(main())
