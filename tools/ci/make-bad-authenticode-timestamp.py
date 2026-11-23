"""Damage the RFC 3161 message imprint in a disposable signed fixture."""

import argparse
from dataclasses import dataclass
from pathlib import Path
import struct

RFC3161 = bytes.fromhex("2b060104018237030301")


@dataclass(frozen=True)
class Element:
    tag: int
    body: int
    end: int


def read_element(data, offset, limit):
    if offset + 2 > limit:
        raise ValueError("truncated DER value")
    tag, size = data[offset : offset + 2]
    cursor = offset + 2
    if size & 0x80:
        width = size & 0x7f
        if not 1 <= width <= 4 or cursor + width > limit:
            raise ValueError("invalid DER size")
        size = int.from_bytes(data[cursor : cursor + width], "big")
        cursor += width
    if cursor + size > limit:
        raise ValueError("DER value exceeds its parent")
    return Element(tag, cursor, cursor + size)


def children(data, element, tag):
    if element.tag != tag:
        raise ValueError(f"expected DER tag {tag:#x}, received {element.tag:#x}")
    fields = []
    cursor = element.body
    while cursor < element.end:
        member = read_element(data, cursor, element.end)
        fields.append(member)
        cursor = member.end
    return fields


def only(data, element, tag):
    (value,) = children(data, element, tag)
    return value


def signed_data(data, content_info):
    _, content = children(data, content_info, 0x30)
    return children(data, only(data, content, 0xa0), 0x30)


def stamp_digest_offset(data):
    if data[:2] != b"MZ":
        raise ValueError("expected a Windows PE fixture")
    pe = struct.unpack_from("<I", data, 60)[0]
    if data[pe : pe + 4] != bytes.fromhex("50450000"):
        raise ValueError("invalid PE header")
    optional = pe + 24
    format_id = struct.unpack_from("<H", data, optional)[0]
    directories = {0x10b: 96, 0x20b: 112}.get(format_id)
    if directories is None:
        raise ValueError("expected a signed PE32 or PE32+ fixture")
    certificate, length = struct.unpack_from("<II", data, optional + directories + 8 * 4)
    entry_length, _, kind = struct.unpack_from("<IHH", data, certificate)
    if kind != 2 or not 8 < entry_length <= length or certificate + length > len(data):
        raise ValueError("invalid PKCS#7 certificate entry")
    content = read_element(data, certificate + 8, certificate + entry_length)
    signer_set = signed_data(data, content)[-1]
    attrs = children(data, only(data, signer_set, 0x31), 0x30)[-1]
    timestamps = []
    for attribute in children(data, attrs, 0xa1):
        oid, values = children(data, attribute, 0x30)
        if oid.tag == 0x06 and data[oid.body : oid.end] == RFC3161:
            timestamps.append(only(data, values, 0x31))
    (timestamp,) = timestamps
    _, _, encapsulated, *_ = signed_data(data, timestamp)
    _, encapsulated_value = children(data, encapsulated, 0x30)
    encoded_info = only(data, encapsulated_value, 0xa0)
    if encoded_info.tag != 0x04:
        raise ValueError("timestamp TSTInfo was not an octet string")
    tst_info = read_element(data, encoded_info.body, encoded_info.end)
    _, _, imprint, *_ = children(data, tst_info, 0x30)
    _, digest = children(data, imprint, 0x30)
    if digest.tag != 0x04 or digest.end - digest.body != 32:
        raise ValueError("expected a SHA-256 message imprint")
    return digest.body


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("genuine", type=Path)
    parser.add_argument("fixture", type=Path)
    args = parser.parse_args()
    data = bytearray(args.genuine.read_bytes())
    offset = stamp_digest_offset(data)
    data[offset] ^= 1
    with args.fixture.open("xb") as output:
        output.write(data)


if __name__ == "__main__":
    main()
