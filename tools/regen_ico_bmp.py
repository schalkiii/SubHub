#!/usr/bin/env python3
"""Regenerate an ICO file replacing PNG-compressed entries with uncompressed
BMP (BGRA, bottom-up) entries. PNG-compressed ICOs are a known cause of the
Tauri v2 Windows taskbar icon failing to render, because the window class
icon is loaded from the embedded resource and PNG-only ICOs are not always
honoured. A BMP-compressed ICO is the canonical Windows format.

Stdlib only (zlib for PNG inflate + manual scanline unfilter).
"""
import struct
import zlib

SRC = r"D:/workspace/sub-aggregate/app/icons/icon.ico"
OUT = r"D:/workspace/sub-aggregate/app/icons/icon.ico"


def decode_png(data: bytes) -> (int, int, bytes):
    """Decode an 8-bit RGBA/RGB PNG to (width, height, BGRA bytes, bottom-up)."""
    assert data[:8] == b"\x89PNG\r\n\x1a\n", "not PNG"
    pos = 8
    width = height = None
    bit_depth = color_type = None
    idat = b""
    while pos < len(data):
        length = struct.unpack(">I", data[pos:pos + 4])[0]
        ctype = data[pos + 4:pos + 8]
        chunk = data[pos + 8:pos + 8 + length]
        if ctype == b"IHDR":
            width, height, bit_depth, color_type = struct.unpack(">IIBB", chunk[:10])
        elif ctype == b"IDAT":
            idat += chunk
        elif ctype == b"IEND":
            break
        pos += 12 + length
    assert bit_depth == 8, f"unsupported bit depth {bit_depth}"
    assert color_type in (2, 6), f"unsupported color type {color_type}"
    channels = 3 if color_type == 2 else 4
    raw = zlib.decompress(idat)
    stride = width * channels
    # unfilter scanlines
    out = bytearray(height * stride)
    prev = bytearray(stride)
    p = 0
    for y in range(height):
        ftype = raw[p]
        p += 1
        line = bytearray(raw[p:p + stride])
        p += stride
        for x in range(stride):
            a = line[x - channels] if x >= channels else 0
            b = prev[x]
            c = prev[x - channels] if x >= channels else 0
            if ftype == 1:
                line[x] = (line[x] + a) & 0xFF
            elif ftype == 2:
                line[x] = (line[x] + b) & 0xFF
            elif ftype == 3:
                line[x] = (line[x] + ((a + b) >> 1)) & 0xFF
            elif ftype == 4:
                pp = a + b - c
                pa = abs(pp - a); pb = abs(pp - b); pc = abs(pp - c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[x] = (line[x] + pr) & 0xFF
            # ftype 0: none
        out[y * stride:(y + 1) * stride] = line
        prev = line
    # convert to BGRA bottom-up
    bgra = bytearray(width * height * 4)
    dp = 0
    for y in range(height - 1, -1, -1):
        sp = y * stride
        for x in range(width):
            r = out[sp]; g = out[sp + 1]; bch = out[sp + 2]
            a = out[sp + 3] if channels == 4 else 255
            bgra[dp:dp + 4] = bytes((bch, g, r, a))
            dp += 4
            sp += channels
    return width, height, bytes(bgra)


def bmp_ico_entry(bgra: bytes, w: int, h: int) -> bytes:
    """Build a BMP-compressed ICO directory entry body (DIB + AND mask)."""
    bpp = 32
    header = struct.pack("<IiiHHIIiiII", 40, w, h * 2, 1, bpp, 0,
                         len(bgra) + ((w + 31) // 32 * 4) * h, 0, 0, 0, 0)
    # AND mask: 1 bpp, zero (fully opaque), rows padded to 4 bytes
    mask_row = (w + 31) // 32 * 4
    and_mask = b"\x00" * (mask_row * h)
    return header + bgra + and_mask


def main():
    data = open(SRC, "rb").read()
    assert data[:4] == b"\x00\x00\x01\x00"
    n = struct.unpack("<H", data[4:6])[0]
    entries = []
    for i in range(n):
        off = 6 + i * 16
        w, h, _, _, _, _, size, dataoff = struct.unpack("<BBBBHHII", data[off:off + 16])
        ww = w if w else 256
        hh = h if h else 256
        body = data[dataoff:dataoff + size]
        if body[:4] == b"\x89PNG":
            pw, ph, pbgra = decode_png(body)
            entries.append((ww, hh, bmp_ico_entry(pbgra, pw, ph)))
        else:
            # already BMP; keep as-is (re-wrap so offsets recompute below)
            entries.append((ww, hh, body))
    # reassemble
    out = bytearray()
    out += b"\x00\x00\x01\x00"
    out += struct.pack("<H", len(entries))
    dir_off = 6 + len(entries) * 16
    data_blob = bytearray()
    for (ww, hh, body) in entries:
        out += struct.pack("<BBBBHHII", ww if ww < 256 else 0, hh if hh < 256 else 0,
                           0, 0, 1, 32, len(body), dir_off + len(data_blob))
        data_blob += body
    out += data_blob
    open(OUT, "wb").write(out)
    print(f"wrote {OUT}: {len(entries)} entries, {len(out)} bytes")


if __name__ == "__main__":
    main()
