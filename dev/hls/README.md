# HLS fixture

This directory contains a local, synthetic HLS fixture for development and test
work on the HLS manifest and prebuffer path.

Tracked files:

- `sl.m3u8` - master playlist with only local relative variant URIs
- `0150_vod.m3u8` through `3340_vod.m3u8` - local variant playlists
- `generate_fixture.py` - recreates the local segment files used by those playlists

The segment files are intentionally not committed. Run the generator to create
them under `dev/hls/segments/`:

```bash
python3 dev/hls/generate_fixture.py
```

The generator writes small synthetic MPEG-TS-style segments produced entirely in
this repository. There is no third-party media payload and no external download
step or redistribution dependency.

Tests do not require these repository-local files. The Rust HLS prebuffer tests
generate the same fixture in a temporary directory by default, or they can use a
custom fixture root via `MEDIAFLOW_HLS_FIXTURE_DIR=/absolute/path`.
