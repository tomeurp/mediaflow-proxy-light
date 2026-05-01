#!/usr/bin/env python3

from __future__ import annotations

from pathlib import Path


VARIANTS = [
    ("0150_vod.m3u8", 165135, "416x234", "avc1.42e00d,mp4a.40.2"),
    ("0240_vod.m3u8", 262346, "480x270", "avc1.42e015,mp4a.40.2"),
    ("0440_vod.m3u8", 481677, "640x360", "avc1.4d401e,mp4a.40.2"),
    ("0640_vod.m3u8", 688301, "640x360", "avc1.4d401e,mp4a.40.2"),
    ("1240_vod.m3u8", 1308077, "960x540", "avc1.4d401f,mp4a.40.2"),
    ("1840_vod.m3u8", 1927853, "1280x720", "avc1.4d401f,mp4a.40.2"),
    ("2540_vod.m3u8", 2650941, "1920x1080", "avc1.640028,mp4a.40.2"),
    ("3340_vod.m3u8", 3477293, "1920x1080", "avc1.640028,mp4a.40.2"),
]

SEGMENT_COUNT = 5
PACKET_SIZE = 188
PACKETS_PER_SEGMENT = 64


def synthetic_ts_segment(seed: int) -> bytes:
    segment = bytearray(PACKET_SIZE * PACKETS_PER_SEGMENT)
    for packet_index in range(PACKETS_PER_SEGMENT):
        offset = packet_index * PACKET_SIZE
        packet = memoryview(segment)[offset : offset + PACKET_SIZE]
        packet[0] = 0x47
        packet[1] = 0x40 | (packet_index & 0x1F)
        packet[2] = (packet_index + seed) & 0xFF
        packet[3] = 0x10
        for payload_index in range(4, PACKET_SIZE):
            packet[payload_index] = (seed + packet_index + payload_index) & 0xFF
    return bytes(segment)


def write_master(root: Path) -> None:
    lines = ["#EXTM3U"]
    for playlist, bandwidth, resolution, codecs in VARIANTS:
        lines.append(
            f'#EXT-X-STREAM-INF:BANDWIDTH={bandwidth},RESOLUTION={resolution},CODECS="{codecs}"'
        )
        lines.append(playlist)
    root.joinpath("sl.m3u8").write_text("\n".join(lines) + "\n", encoding="utf-8")


def write_variants(root: Path) -> None:
    body = [
        "#EXTM3U",
        "#EXT-X-VERSION:3",
        "#EXT-X-PLAYLIST-TYPE:VOD",
        "#EXT-X-TARGETDURATION:4.000",
        "#EXT-X-MEDIA-SEQUENCE:0",
    ]
    for idx in range(SEGMENT_COUNT):
        body.append("#EXTINF:4.000,")
        body.append(f"segments/seg{idx:03}.ts")
    body.append("#EXT-X-ENDLIST")
    text = "\n".join(body) + "\n"

    for playlist, *_ in VARIANTS:
        root.joinpath(playlist).write_text(text, encoding="utf-8")


def write_segments(root: Path) -> None:
    segments_dir = root / "segments"
    segments_dir.mkdir(parents=True, exist_ok=True)
    for idx in range(SEGMENT_COUNT):
        segments_dir.joinpath(f"seg{idx:03}.ts").write_bytes(synthetic_ts_segment(idx))


def main() -> None:
    root = Path(__file__).resolve().parent
    write_master(root)
    write_variants(root)
    write_segments(root)


if __name__ == "__main__":
    main()
