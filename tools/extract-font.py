#!/usr/bin/env python3
"""Достаёт растровые глифы из PCF в таблицу для components/label/src/font.rs.

Запускается руками и редко: результат коммитится, чтобы сборка не зависела ни от
сети, ни от установленного xfonts-base. Источник — font-misc-misc, public domain
("Public domain font.  Share and enjoy.", /usr/share/doc/xfonts-base/copyright).
"""
import gzip
import struct
import sys

PCF_METRICS = 4
PCF_BITMAPS = 8
PCF_BDF_ENCODINGS = 32
PCF_COMPRESSED_METRICS = 0x100
PCF_BYTE_MASK = 0x4


def tables(blob):
    if blob[:4] != b"\x01fcp":
        raise SystemExit("не PCF")
    (count,) = struct.unpack_from("<i", blob, 4)
    out = {}
    for i in range(count):
        kind, fmt, size, offset = struct.unpack_from("<4i", blob, 8 + 16 * i)
        out[kind] = (fmt, size, offset)
    return out


def reader(blob, offset, fmt):
    big = bool(fmt & PCF_BYTE_MASK)
    order = ">" if big else "<"

    def read(spec, at):
        return struct.unpack_from(order + spec, blob, at)

    return read


def read_metrics(blob, fmt, offset):
    read = reader(blob, offset, fmt)
    at = offset + 4
    if fmt & PCF_COMPRESSED_METRICS:
        (count,) = read("h", at)
        at += 2
        metrics = []
        for _ in range(count):
            left, right, width, ascent, descent = struct.unpack_from("5B", blob, at)
            at += 5
            metrics.append(
                (left - 0x80, right - 0x80, width - 0x80, ascent - 0x80, descent - 0x80)
            )
        return metrics
    (count,) = read("i", at)
    at += 4
    metrics = []
    for _ in range(count):
        left, right, width, ascent, descent, _attrs = read("5hH", at)
        at += 12
        metrics.append((left, right, width, ascent, descent))
    return metrics


def read_bitmaps(blob, fmt, offset):
    read = reader(blob, offset, fmt)
    (count,) = read("i", offset + 4)
    offsets = [read("i", offset + 8 + 4 * i)[0] for i in range(count)]
    sizes = read("4i", offset + 8 + 4 * count)
    padding = fmt & 3
    data_start = offset + 8 + 4 * count + 16
    data = blob[data_start : data_start + sizes[padding]]
    return offsets, data, 1 << padding, bool(fmt & PCF_BYTE_MASK)


def read_encodings(blob, fmt, offset):
    read = reader(blob, offset, fmt)
    min2, max2, min1, max1, _default = read("5h", offset + 4)
    at = offset + 14
    table = {}
    for byte1 in range(min1, max1 + 1):
        for byte2 in range(min2, max2 + 1):
            (index,) = read("H", at)
            at += 2
            if index != 0xFFFF:
                code = byte2 if min1 == max1 == 0 else (byte1 << 8) | byte2
                table[code] = index
    return table


def main(path, out_path):
    blob = gzip.open(path, "rb").read() if path.endswith(".gz") else open(path, "rb").read()
    t = tables(blob)
    metrics = read_metrics(blob, *(t[PCF_METRICS][0], t[PCF_METRICS][2]))
    offsets, data, pad, big = read_bitmaps(blob, t[PCF_BITMAPS][0], t[PCF_BITMAPS][2])
    encodings = read_encodings(blob, t[PCF_BDF_ENCODINGS][0], t[PCF_BDF_ENCODINGS][2])

    # моноширинность здесь не удобство, а условие: иначе таблица без метрик врала бы о ширине
    boxes = {(m[2], m[3], m[4]) for m in metrics}
    if len(boxes) != 1:
        raise SystemExit(f"шрифт не моноширинный, разных боксов {len(boxes)}: {sorted(boxes)[:5]}")
    width, ascent, descent = boxes.pop()
    height = ascent + descent
    row_bytes = (width + 7) // 8
    stride = pad * ((width + pad * 8 - 1) // (pad * 8))

    wanted = list(range(0x20, 0x7F)) + list(range(0xA1, 0xFD))

    glyphs = []
    for code in wanted:
        index = encodings.get(code)
        if index is None:
            continue
        try:
            char = bytes([code]).decode("iso8859-5")
        except UnicodeDecodeError:
            continue
        start = offsets[index]
        rows = []
        for row in range(height):
            at = start + row * stride
            chunk = data[at : at + row_bytes]
            if not big:
                chunk = bytes(int(f"{b:08b}"[::-1], 2) for b in chunk)
            rows.append(chunk.ljust(row_bytes, b"\x00"))
        glyphs.append((ord(char), rows, height))
    glyphs.sort(key=lambda g: g[0])

    with open(out_path, "w", encoding="utf-8") as out:
        out.write("// СГЕНЕРИРОВАНО tools/extract-font.py, правится не здесь, а там\n")
        out.write("// Источник: font-misc-misc из xfonts-base, public domain\n")
        out.write("// (\"Public domain font.  Share and enjoy.\", /usr/share/doc/xfonts-base/copyright)\n\n")
        out.write(f"pub const GLYPH_WIDTH: usize = {width};\n")
        out.write(f"pub const GLYPH_HEIGHT: usize = {height};\n")
        out.write(f"pub const ROW_BYTES: usize = {row_bytes};\n\n")
        out.write("// код символа Unicode -> строки битовой матрицы, старший бит слева\n")
        out.write(
            f"pub static GLYPHS: [(u32, [u8; GLYPH_HEIGHT * ROW_BYTES]); {len(glyphs)}] = [\n"
        )
        for code, rows, _height in glyphs:
            flat = b"".join(rows)
            flat = flat.ljust(height * row_bytes, b"\x00")[: height * row_bytes]
            body = ", ".join(f"0x{b:02x}" for b in flat)
            out.write(f"    ({code}, [{body}]),\n")
        out.write("];\n")
    print(f"{len(glyphs)} глифов {width}x{height}, {row_bytes} байт на строку -> {out_path}")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("использование: extract-font.py <шрифт.pcf.gz> <выход.rs>")
    main(sys.argv[1], sys.argv[2])
