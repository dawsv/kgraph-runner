"""Standard-library PNG decoding and screenshot verification."""

from pathlib import Path
import struct
from typing import Tuple
import zlib


_PNG_SIGNATURE = b"\x89PNG\r\n\x1a\n"
_CHANNELS = {0: 1, 2: 3, 3: 1, 4: 2, 6: 4}
_IMAGE_ERRORS = (OSError, ValueError, struct.error, zlib.error)


def _paeth(left, up, upper_left):
    estimate = left + up - upper_left
    a, b, c = abs(estimate - left), abs(estimate - up), abs(estimate - upper_left)
    if a <= b and a <= c:
        return left
    return up if b <= c else upper_left


def _unfilter_rows(raw, height, stride, channels):
    previous = bytearray(stride)
    for offset in range(0, height * (stride + 1), stride + 1):
        method = raw[offset]
        row = bytearray(raw[offset + 1:offset + 1 + stride])
        if method > 4:
            raise ValueError(f"Unknown PNG filter {method}")
        if method:
            for index, value in enumerate(row):
                left = row[index - channels] if index >= channels else 0
                up = previous[index]
                if method == 1:
                    predictor = left
                elif method == 2:
                    predictor = up
                elif method == 3:
                    predictor = (left + up) // 2
                else:
                    upper_left = previous[index - channels] if index >= channels else 0
                    predictor = _paeth(left, up, upper_left)
                row[index] = (value + predictor) & 255
        yield row
        previous = row


def _png_chunks(data):
    if not data.startswith(_PNG_SIGNATURE):
        raise ValueError("Missing PNG signature")
    offset = len(_PNG_SIGNATURE)
    while offset + 12 <= len(data):
        length, kind = struct.unpack_from(">I4s", data, offset)
        end = offset + 8 + length
        if end + 4 > len(data):
            raise ValueError(f"Truncated {kind!r} chunk")
        payload = data[offset + 8:end]
        if zlib.crc32(kind + payload) != struct.unpack_from(">I", data, end)[0]:
            raise ValueError(f"Bad CRC in {kind!r} chunk")
        yield kind, payload
        offset = end + 4
        if kind == b"IEND":
            if payload or offset != len(data):
                raise ValueError("Invalid PNG end")
            return
    raise ValueError("Missing IEND chunk")


def _decode_png(data):
    chunks = _png_chunks(data)
    kind, header = next(chunks)
    if kind != b"IHDR" or len(header) != 13:
        raise ValueError("Missing or invalid IHDR chunk")
    width, height, depth, color_type, compression, filter_method, interlace = struct.unpack(">IIBBBBB", header)
    channels = _CHANNELS.get(color_type)
    if not width or not height or width > 0x7fffffff or height > 0x7fffffff:
        raise ValueError("Invalid PNG dimensions")
    if depth != 8 or not channels or compression or filter_method or interlace:
        raise ValueError("Expected an 8-bit, non-interlaced PNG")
    compressed = bytearray()
    palette = transparency = None
    idat_ended = False
    for kind, payload in chunks:
        if kind == b"IDAT":
            if idat_ended:
                raise ValueError("PNG IDAT chunks must be consecutive")
            compressed.extend(payload)
            continue
        if compressed:
            idat_ended = True
        if kind == b"PLTE":
            if compressed or palette is not None or transparency is not None or color_type in (0, 4):
                raise ValueError("Unexpected PNG palette")
            if len(payload) % 3 or not 3 <= len(payload) <= 768:
                raise ValueError("Invalid PNG palette")
            palette = payload
        elif kind == b"tRNS":
            if compressed or transparency is not None:
                raise ValueError("Unexpected PNG transparency chunk")
            transparency = payload
        elif kind != b"IEND" and not kind[0] & 32:
            raise ValueError(f"Unsupported critical chunk {kind!r}")

    if color_type == 3 and palette is None:
        raise ValueError("Indexed PNG requires a palette")
    palette_size = len(palette or b"") // 3
    if transparency is not None:
        valid_lengths = {0: (2,), 2: (6,), 3: range(1, palette_size + 1)}
        if len(transparency) not in valid_lengths.get(color_type, ()):
            raise ValueError("Invalid PNG transparency data")
        if color_type in (0, 2):
            samples = struct.unpack(">H" if color_type == 0 else ">3H", transparency)
            if any(sample > 255 for sample in samples):
                raise ValueError("PNG transparency key exceeds bit depth")
            transparency = bytes(samples)

    stride = width * channels
    size = height * (stride + 1)
    # Bound expansion by the declared scanline size. For impossibly large
    # headers, zlib's own compression ratio provides an additional bound.
    decoder = zlib.decompressobj()
    raw = decoder.decompress(compressed, min(size + 1, len(compressed) * 1032 + 1))
    if len(raw) != size or not decoder.eof or decoder.unused_data:
        raise ValueError("PNG scanline data does not match the dimensions")
    if any(raw[offset] > 4 for offset in range(0, size, stride + 1)):
        raise ValueError("Unknown PNG filter")
    return width, height, color_type, palette, transparency, raw


def _decode_alpha(data):
    width, height, color_type, palette, transparency, raw = _decode_png(data)
    channels = _CHANNELS[color_type]
    stride = width * channels
    has_alpha = color_type in (4, 6) or transparency is not None
    if not has_alpha and color_type != 3:
        return width, height, color_type, has_alpha, [bytes([255]) * width] * height

    if color_type == 3:
        table = (transparency or b"").ljust(len(palette) // 3, b"\xff")

    alpha_rows = []
    for row in _unfilter_rows(raw, height, stride, channels):
        if color_type in (4, 6):
            alpha = row[channels - 1::channels]
        elif color_type == 3:
            try:
                alpha = bytes(table[index] for index in row)
            except IndexError as exc:
                raise ValueError("PNG pixel refers to a missing palette entry") from exc
        else:
            alpha = bytes(0 if row[index:index + channels] == transparency else 255
                          for index in range(0, stride, channels))
        alpha_rows.append(alpha)
    return width, height, color_type, has_alpha, alpha_rows


def _has_visible_variation(width, height, color_type, palette, transparency, raw):
    channels = _CHANNELS[color_type]
    rows = _unfilter_rows(raw, height, width * channels, channels)
    if color_type in (0, 2):
        first_row = next(rows)
        uniform_row = first_row[:channels] * width
        return first_row != uniform_row or any(row != uniform_row for row in rows)
    if color_type == 3:
        alphas = (transparency or b"").ljust(len(palette) // 3, b"\xff")
        colors = [palette[index * 3:(index + 1) * 3] + bytes([alpha]) if alpha else b""
                  for index, alpha in enumerate(alphas)]
        seen = set()
        for row in rows:
            seen.update(row)
        if max(seen) >= len(colors):
            raise ValueError("PNG pixel refers to a missing palette entry")
        return len({colors[index] for index in seen}) > 1

    first_pixel = None
    for row in rows:
        for index in range(0, len(row), channels):
            pixel = row[index:index + channels] if row[index + channels - 1] else b""
            if first_pixel is None:
                first_pixel = pixel
            elif pixel != first_pixel:
                return True
    return False


def verify_png_file(path: Path, *, min_width: int = 16, min_height: int = 16) -> Tuple[bool, str, str]:
    """Verify that a screenshot decodes, meets the dimensions, and has visible variation."""
    path = Path(path)
    if not path.is_file():
        return False, "missing_file", f"Missing screenshot {path}"
    try:
        decoded = _decode_png(path.read_bytes())
        has_variation = _has_visible_variation(*decoded)
    except _IMAGE_ERRORS as exc:
        return False, "invalid_png", f"{path}: {exc}"
    width, height = decoded[:2]
    if width < min_width or height < min_height:
        return False, "too_small", f"{path}: {width}×{height}px, minimum {min_width}×{min_height}px"
    if not has_variation:
        return False, "blank_png", f"{path}: screenshot is blank or a solid color"
    return True, "", f"{width}×{height}px PNG with visible variation"


def inspect_png(path: Path) -> dict:
    """Decode an 8-bit non-interlaced PNG and measure its alpha coverage."""
    width, height, color_type, has_alpha, rows = _decode_alpha(Path(path).read_bytes())
    # Count each border pixel once, including images only one pixel wide or high.
    border = [rows[0]]
    if height > 1:
        border.append(rows[-1])
        border.extend(row[:1] + (row[-1:] if width > 1 else b"") for row in rows[1:-1])
    edge_ratio = sum(alpha <= 16 for row in border for alpha in row) / sum(map(len, border))

    min_x, min_y, max_x, max_y = width, height, -1, -1
    opaque = transparent = 0
    for y, row in enumerate(rows):
        for x, alpha in enumerate(row):
            if alpha <= 16:
                transparent += 1
                continue
            opaque += alpha == 255
            min_x, max_x = min(x, min_x), max(x, max_x)
            min_y, max_y = min(y, min_y), y

    bbox = None
    bottom_margin = 1.0
    shadow_pixels = 0
    if max_y >= 0:
        content_height = max_y - min_y + 1
        bbox = dict(min_x=min_x, min_y=min_y, max_x=max_x, max_y=max_y,
                    content_width=max_x - min_x + 1, content_height=content_height)
        bottom_margin = (height - max_y - 1) / height
        shadow_band = (content_height * 3 + 9) // 10
        shadow_pixels = sum(10 <= alpha <= 200
                            for row in rows[max_y + 1 - shadow_band:max_y + 2]
                            for alpha in row[min_x:max_x + 1])

    return dict(width=width, height=height, color_type=color_type, has_alpha=has_alpha,
                aspect_ratio=width / height, opaque_ratio=opaque / (width * height),
                transparent_ratio=transparent / (width * height), edge_transparent_ratio=edge_ratio,
                bbox=bbox, bottom_margin_ratio=bottom_margin, soft_shadow_pixels=shadow_pixels)
