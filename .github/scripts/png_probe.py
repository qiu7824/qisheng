"""Detect an empty native panel capture without external image dependencies."""
import struct
import zlib


def opaque_color_count(path, stop_after=64):
    data = path.read_bytes()
    assert data[:8] == b'\x89PNG\r\n\x1a\n', 'Not a PNG capture'
    width, height, depth, kind, compression, filtering, interlace = struct.unpack('>IIBBBBB', data[16:29])
    assert 32 <= width <= 4096 and 32 <= height <= 4096
    assert depth == 8 and kind in (2, 6) and compression == filtering == interlace == 0
    encoded = bytearray()
    offset = 8
    while offset + 12 <= len(data):
        length = struct.unpack('>I', data[offset:offset + 4])[0]
        assert offset + length + 12 <= len(data)
        if data[offset + 4:offset + 8] == b'IDAT':
            encoded.extend(data[offset + 8:offset + 8 + length])
        offset += length + 12
    channels = 4 if kind == 6 else 3
    stride = width * channels
    expected = height * (stride + 1)
    decoder = zlib.decompressobj()
    pixels = decoder.decompress(encoded, expected + 1)
    assert len(pixels) == expected and decoder.eof, 'Incomplete or oversized PNG raster'
    previous = bytearray(stride)
    colors = set()
    for y in range(height):
        offset = y * (stride + 1)
        method = pixels[offset]
        row = bytearray(pixels[offset + 1:offset + 1 + stride])
        assert method <= 4
        for i in range(stride):
            a = row[i - channels] if i >= channels else 0
            b = previous[i]
            c = previous[i - channels] if i >= channels else 0
            if method == 1:
                predictor = a
            elif method == 2:
                predictor = b
            elif method == 3:
                predictor = (a + b) // 2
            elif method == 4:
                estimate = a + b - c
                distances = (abs(estimate - a), abs(estimate - b), abs(estimate - c))
                predictor = (a, b, c)[distances.index(min(distances))]
            else:
                predictor = 0
            row[i] = (row[i] + predictor) & 255
        if height // 20 <= y < height * 19 // 20 and y % 3 == 0:
            for x in range(width // 20, width * 19 // 20, 3):
                i = x * channels
                if channels == 3 or row[i + 3] == 255:
                    colors.add(bytes(row[i:i + 3]))
                    if len(colors) >= stop_after:
                        return len(colors)
        previous = row
    return len(colors)
