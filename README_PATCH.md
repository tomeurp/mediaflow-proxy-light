# MediaFlow Proxy Light DASH/HLS fixes

This patch ports the fixes from the Python MediaFlow Proxy work into the Rust Light implementation.

## Included fixes

1. DASH BaseURL inheritance
   - Parse BaseURL at MPD, Period, AdaptationSet and Representation levels.
   - Resolve inherited BaseURL via chained `resolve_url`/urljoin semantics.
   - Absolute CDN BaseURLs correctly replace the MPD origin.

2. Correct segment/init URL resolution
   - SegmentTemplate `media` and `initialization` are resolved against the inherited BaseURL, not directly against the MPD URL.
   - SegmentList and SegmentBase also use the inherited BaseURL.

3. VOD fMP4 EXT-X-MAP behavior verified
   - Light already had `let use_map = !is_ts_mode;`, which applies EXT-X-MAP to both live and VOD fMP4.
   - This avoids re-sending init/moov in every segment.

4. WebVTT subtitle support
   - Parse `text/vtt` AdaptationSets, including AdaptationSets with a BaseURL but no Representation.
   - Add subtitle profiles to the HLS master as `EXT-X-MEDIA:TYPE=SUBTITLES`.
   - Add `SUBTITLES="subs"` to video variants.
   - Emit a simple HLS WebVTT playlist whose segment is proxied through `/proxy/stream` to avoid client-side CORS issues.

## Files

- `src/mpd/parser.rs`
- `src/mpd/processor.rs`

Copy these over the same paths in a fork of `mhdzumair/MediaFlow-Proxy-Light`, then run:

```bash
cargo fmt
cargo test
cargo build --release
```
