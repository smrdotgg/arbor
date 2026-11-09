use super::*;
use crate::app::tests::state_in;
use crate::commands::Cmd;
use rune::{Key, KeyEvent, Mods, Tx};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

fn folder(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("arbor-preview-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn never() -> AtomicBool {
    AtomicBool::new(false)
}

// ---- sniffing and text ----------------------------------------------------------------

#[test]
fn text_is_told_from_binary_by_its_bytes() {
    assert_eq!(sniff_text(b"fn main() {}\n\tok\r\n", false), Some(Encoding::Utf8));
    assert_eq!(sniff_text("naïve café ✓".as_bytes(), false), Some(Encoding::Utf8));
    assert_eq!(sniff_text(b"\xEF\xBB\xBFhello", false), Some(Encoding::Utf8Bom));
    assert_eq!(sniff_text(b"\xFF\xFEh\0i\0", false), Some(Encoding::Utf16Le));
    assert_eq!(sniff_text(b"\xFE\xFF\0h\0i", false), Some(Encoding::Utf16Be));
    // Latin-1 (or Windows-1252): 8-bit text that isn't UTF-8, however many accents.
    assert_eq!(sniff_text(b"Caf\xe9 cr\xe8me br\xfbl\xe9e and a lot more plain text here", false), Some(Encoding::Windows1252));
    assert_eq!(sniff_text(b"Caf\xe9 cr\xe8me br\xfbl\xe9e", false), Some(Encoding::Windows1252), "23% accented: still text");
    // UTF-8 with a stray invalid byte: still UTF-8, the byte shown as U+FFFD.
    assert_eq!(
        sniff_text(&[b"caf\xc3\xa9 cr\xc3\xa8me and some plain text, then a stray ".as_slice(), b"\xff byte"].concat(), false),
        Some(Encoding::Lossy)
    );
    // A character cut by the bounded read is still UTF-8.
    let cut = "a long line ending in é".as_bytes();
    assert_eq!(sniff_text(&cut[..cut.len() - 1], true), Some(Encoding::Utf8));
    // NULs, control characters, or bytes Windows-1252 leaves undefined: binary.
    assert_eq!(sniff_text(b"ELF\0\0\0\x01\x02 rest", false), None);
    assert_eq!(sniff_text(&(0u8..=255).cycle().take(4096).collect::<Vec<_>>(), false), None);
    assert_eq!(sniff_text(&[0x8Du8, 0x41, 0x81, 0x9D, 0x42, 0x90, 0x8F, 0x43].repeat(100), false), None);
    // Invalid UTF-8 with no control bytes reads as 8-bit text ("Ã(\u{A0}¡â(¡A").
    assert_eq!(sniff_text(&[0xC3u8, 0x28, 0xA0, 0xA1, 0xE2, 0x28, 0xA1, 0x41].repeat(100), false), Some(Encoding::Windows1252));
    assert_eq!(sniff_text(b"", false), Some(Encoding::Utf8));
}

#[test]
fn text_lays_out_lines_and_says_what_it_left_out() {
    let t = text(b"one\r\n\ttwo\nthree\n", false, 17).unwrap();
    assert_eq!((0..t.lines.len()).map(|i| t.line(i)).collect::<Vec<_>>(), ["one", "    two", "three"]);
    assert_eq!((t.cols, t.note.clone()), (7, None));
    // A control character shows as U+FFFD; a long line is cut.
    let long = format!("a\x07b\n{}", "x".repeat(MAX_COLS + 50));
    let t = text(long.as_bytes(), false, long.len() as u64).unwrap();
    assert_eq!(t.line(0), "a\u{FFFD}b");
    assert_eq!(t.line(1).chars().count(), MAX_COLS + 1);
    assert!(t.line(1).ends_with('…'));
    assert_eq!(t.note.as_deref(), Some("Lines over 1000 characters are cut"));
    // Truncated, cut mid-character: the note, and no stray U+FFFD at the end.
    let mut bytes = "é".repeat(10).into_bytes();
    bytes.pop();
    let t = text(&bytes, true, 3_400_000).unwrap();
    assert_eq!(t.line(0), "é".repeat(9));
    assert_eq!(t.note.as_deref(), Some("Showing the first 256 KB of 3.4 MB"));
    let t = text(b"Caf\xe9 and a lot more plain text to make it count", false, 40).unwrap();
    assert!(t.line(0).starts_with("Café and"));
    assert_eq!(t.note.as_deref(), Some("Shown as Windows-1252"));
    let t = text(b"caf\xc3\xa9 and plain text with one stray \xff byte in it", false, 40).unwrap();
    assert!(t.line(0).contains("café") && t.line(0).contains('\u{FFFD}'));
    assert_eq!(t.note.as_deref(), Some("Not valid UTF-8: invalid bytes show as �"));
    let t = text(b"\xFF\xFEh\0i\0\n\0", false, 8).unwrap();
    assert_eq!((t.line(0), t.note.as_deref()), ("hi", Some("UTF-16")));
    assert!(text(b"", false, 0).unwrap().lines.is_empty());
    assert!(text(b"\0\0\0\0binary", false, 10).is_none());
}

/// The QA's Latin-1 file: "Café crème brûlée" and "naïve résumé" show with their accents
/// (they used to go to Quick Look as binary, which drew them as Mac Roman: "CafÈ"), and the
/// panel says how it read them.
#[test]
fn latin1_text_shows_as_windows_1252() {
    let d = folder("latin1");
    let p = d.join("latin1.txt");
    std::fs::write(&p, b"Caf\xe9 cr\xe8me br\xfbl\xe9e\nna\xefve r\xe9sum\xe9\n\x93quoted\x94 \x80 5\n").unwrap();
    let job = Job { path: p, is_dir: false, is_link: false, ext: "txt".into(), size: 40, fit: DEFAULT_FIT };
    let Body::Text(t) = load(&job, &never()).unwrap().body else { panic!("text") };
    assert_eq!((t.line(0), t.line(1), t.line(2)), ("Café crème brûlée", "naïve résumé", "“quoted” € 5"));
    assert_eq!(t.note.as_deref(), Some("Shown as Windows-1252"));
}

/// The review's case: Latin-1 with no newline at the end ("Caf\xE9" was taken for UTF-8
/// cut short and shown as "Caf�"). Only a read cut at its limit excuses a partial character.
#[test]
fn latin1_at_the_end_of_the_file_is_still_latin1() {
    assert_eq!(sniff_text(b"Caf\xe9", false), Some(Encoding::Windows1252));
    assert_eq!(sniff_text(b"Caf\xc3", false), Some(Encoding::Windows1252), "a lone lead byte at the real end too");
    assert_eq!(sniff_text("café".as_bytes(), false), Some(Encoding::Utf8));
    assert_eq!(sniff_text(&"café".as_bytes()[..4], true), Some(Encoding::Utf8), "cut by the bounded read mid-é");
    let d = folder("latin1-eof");
    let p = d.join("cafe.txt");
    std::fs::write(&p, b"Caf\xe9").unwrap();
    let job = Job { path: p, is_dir: false, is_link: false, ext: "txt".into(), size: 4, fit: DEFAULT_FIT };
    let Body::Text(t) = load(&job, &never()).unwrap().body else { panic!("text") };
    assert_eq!((t.line(0), t.note.as_deref()), ("Café", Some("Shown as Windows-1252")));
}

/// A 1 MB text file: only the bounded head is read, and the panel says so.
#[test]
fn a_big_text_file_reads_only_its_head() {
    let d = folder("bigtext");
    let p = d.join("big.txt");
    std::fs::write(&p, "0123456789abcde\n".repeat(65_536)).unwrap();
    let job = Job { path: p, is_dir: false, is_link: false, ext: "txt".into(), size: 1_048_576, fit: DEFAULT_FIT };
    let Body::Text(t) = load(&job, &never()).unwrap().body else { panic!("text") };
    assert_eq!(t.lines.len(), TEXT_MAX / 16);
    assert_eq!(t.note.as_deref(), Some("Showing the first 256 KB of 1.0 MB"));
}

// ---- images ---------------------------------------------------------------------------

#[test]
fn images_are_told_by_their_first_bytes() {
    assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\n...."), Some(Format::Png));
    assert_eq!(sniff_image(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(Format::Jpeg));
    assert_eq!(sniff_image(b"GIF89a.."), Some(Format::Gif));
    assert_eq!(sniff_image(b"RIFF\0\0\0\0WEBPVP8 "), Some(Format::Webp));
    assert_eq!(sniff_image(&[b"BM".as_slice(), &[0; 30]].concat()), Some(Format::Bmp));
    assert_eq!(sniff_image(b"BM"), None, "too short for a BMP header");
    assert_eq!(sniff_image(b"RIFF\0\0\0\0WAVEfmt "), None);
    assert_eq!(sniff_image(b"plain text"), None);
}

#[test]
fn size_limits_and_fitting() {
    assert_eq!(too_big(4000, 3000, false), None);
    assert_eq!(too_big(10_000, 10_000, false), None, "100 MP exactly is allowed");
    assert!(too_big(10_001, 10_000, false).unwrap().contains("100 megapixels"));
    assert!(too_big(8000, 7000, true).is_some(), "56 MP decoded whole is over 24");
    assert!(too_big(5000, 5000, true).is_some() && too_big(6000, 4000, true).is_none(), "24 MP decoded whole is the limit");
    assert!(too_big(u32::MAX as u64, u32::MAX as u64, false).is_some(), "no overflow");
    assert_eq!(fit_size(4000, 3000, [1000, 1000]), (1000, 750));
    assert_eq!(fit_size(3000, 4000, [1000, 500]), (375, 500));
    assert_eq!(fit_size(16, 16, [1000, 1000]), (16, 16), "never larger");
    assert_eq!(fit_size(100_000, 1, [100, 100]), (100, 1));
}

#[test]
fn shrinking_averages_boxes_weighted_by_alpha() {
    // 4 × 2 → 2 × 1: each output pixel averages a 2 × 2 box.
    let mut s = Shrink::new(4, 2, 2, 1);
    s.row(0, &[0, 0, 0, 255, 100, 100, 100, 255, 10, 20, 30, 255, 255, 0, 0, 0], 4);
    s.row(1, &[0, 0, 0, 255, 100, 100, 100, 255, 10, 20, 30, 255, 255, 0, 0, 0], 4);
    let out = s.finish();
    assert_eq!(&out[..4], &[50, 50, 50, 255]);
    // Transparent pixels don't darken the colour: they only thin the alpha.
    assert_eq!(&out[4..], &[10, 20, 30, 127]);
    // Gray rows widen to RGBA.
    let mut s = Shrink::new(2, 1, 2, 1);
    s.row(0, &[7, 9], 1);
    assert_eq!(s.finish(), [7, 7, 7, 255, 9, 9, 9, 255]);
}

/// A PNG claiming 20,000 × 20,000 (400 MP) is refused from its header: nothing allocated.
#[test]
fn a_huge_image_is_refused_from_its_header() {
    let mut bytes = Vec::new();
    {
        let mut e = png::Encoder::new(&mut bytes, 20_000, 20_000);
        e.set_color(png::ColorType::Rgba);
        let mut w = e.write_header().unwrap();
        // The header is read up to the first data chunk.
        w.write_chunk(png::chunk::IDAT, &[0; 8]).unwrap();
        drop(w);
    }
    let err = image(Format::Png, std::io::Cursor::new(bytes), DEFAULT_FIT, &never()).unwrap_err();
    assert!(err.contains("400 megapixels") && err.starts_with(TOO_LARGE), "{err}");
}

/// A PNG header with `w` × `h` and one empty data chunk.
fn png_header(w: u32, h: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut e = png::Encoder::new(&mut bytes, w, h);
        e.set_color(png::ColorType::Grayscale);
        let mut wr = e.write_header().unwrap();
        wr.write_chunk(png::chunk::IDAT, &[0; 8]).unwrap();
    }
    bytes
}

/// 99,999,999 × 1 is under the pixel limit, but its row and column map would take a
/// gigabyte: each side is capped too, and the refusal is a card.
#[test]
fn a_very_wide_image_is_refused_from_its_header() {
    let err = image(Format::Png, std::io::Cursor::new(png_header(99_999_999, 1)), DEFAULT_FIT, &never()).unwrap_err();
    assert!(err.starts_with(TOO_LARGE) && err.contains("wider or taller"), "{err}");
    assert!(too_big(65_535, 1, false).is_none() && too_big(65_536, 1, false).is_some() && too_big(1, 70_000, true).is_some());
    let d = folder("wide");
    let p = d.join("wide.png");
    std::fs::write(&p, png_header(99_999_999, 1)).unwrap();
    let job = Job { path: p, is_dir: false, is_link: false, ext: "png".into(), size: 1, fit: DEFAULT_FIT };
    let Body::Card(c) = load(&job, &never()).unwrap().body else { panic!("a card") };
    assert!(c.note.unwrap().starts_with(TOO_LARGE));
}

/// Thousands of ICC segments would pile up in the decoder: the markers are walked first.
#[test]
fn a_jpeg_full_of_metadata_is_refused_before_decoding() {
    use image::ImageEncoder;
    let mut jpg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpg).write_image(&[128; 16 * 16 * 3], 16, 16, image::ExtendedColorType::Rgb8).unwrap();
    // APP2 "ICC_PROFILE" segments of 64 KB each, 300 of them: 19 MB.
    let mut seg = vec![0xFF, 0xE2, 0xFF, 0xFF];
    seg.extend_from_slice(b"ICC_PROFILE\0\x01\x01");
    seg.resize(2 + 0xFFFF, 0);
    let bomb = [&jpg[..2], &seg.repeat(300), &jpg[2..]].concat();
    let err = image(Format::Jpeg, std::io::Cursor::new(&bomb), DEFAULT_FIT, &never()).unwrap_err();
    assert!(err.starts_with(TOO_LARGE) && err.contains("metadata"), "{err}");
    // A few segments are fine, and so is a scan with stuffed bytes and restart markers.
    let ok = [&jpg[..2], &seg.repeat(3), &jpg[2..]].concat();
    assert!(image(Format::Jpeg, std::io::Cursor::new(&ok), DEFAULT_FIT, &never()).unwrap().is_some());
    assert_eq!(jpeg_scan(&mut std::io::Cursor::new(&ok)).unwrap(), Some((16, 16, false)));
    // The size in the frame header is checked before the decoder runs.
    let mut huge = jpg.clone();
    let sof = huge.windows(2).position(|w| w == [0xFF, 0xC0]).unwrap();
    huge[sof + 5..sof + 9].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
    let err = image(Format::Jpeg, std::io::Cursor::new(&huge), DEFAULT_FIT, &never()).unwrap_err();
    assert!(err.starts_with(TOO_LARGE), "{err}");
}

/// A 1 × 1 canvas around a 16,000 × 16,000 bitstream: refused from the bitstream's header,
/// never decoded.
#[test]
fn a_webp_hiding_a_bigger_image_is_refused_before_decoding() {
    let chunk =
        |id: &[u8], body: &[u8]| [id, &(body.len() as u32).to_le_bytes(), body, if body.len() % 2 == 1 { &[0] } else { &[] }].concat();
    // VP8X: flags, canvas 1 × 1 (each stored minus one, 24 bits).
    let vp8x = chunk(b"VP8X", &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    // VP8: a key frame tag, the start code, 16,000 × 16,000, and a little filler.
    let mut vp8 = vec![0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a];
    vp8.extend_from_slice(&16_000u16.to_le_bytes());
    vp8.extend_from_slice(&16_000u16.to_le_bytes());
    vp8.extend_from_slice(&[0; 64]);
    let body = [b"WEBP".as_slice(), &vp8x, &chunk(b"VP8 ", &vp8)].concat();
    let file = [b"RIFF".as_slice(), &(body.len() as u32).to_le_bytes(), &body].concat();
    assert_eq!(webp_frames(&mut std::io::Cursor::new(&file)).unwrap(), [(16_000, 16_000)]);
    let err = image(Format::Webp, std::io::Cursor::new(&file), DEFAULT_FIT, &never()).unwrap_err();
    assert!(err.starts_with(TOO_LARGE), "{err}");
    // An ordinary WebP's bitstream is its own size.
    let d = folder("webp-frames");
    let imgs = write_images(&d, 64, 48);
    let real = std::fs::read(&imgs[3].0).unwrap();
    assert_eq!(webp_frames(&mut std::io::Cursor::new(&real)).unwrap(), [(64, 48)]);
}

/// A GIF of `w` × `h` on a canvas of `cw` × `ch` at (`left`, `top`), grey levels from
/// `gradient`, optionally interlaced.
fn gif_file(cw: u16, ch: u16, (w, h, left, top): (u16, u16, u16, u16), interlaced: bool) -> Vec<u8> {
    let px = gradient(w as u32, h as u32);
    let palette: Vec<u8> = (0..=255u8).flat_map(|i| [i, i, i]).collect();
    let mut b = Vec::new();
    {
        let mut e = gif::Encoder::new(&mut b, cw, ch, &palette).unwrap();
        let idx: Vec<u8> = px.as_chunks::<4>().0.iter().map(|p| p[0]).collect();
        let mut f = gif::Frame::from_indexed_pixels(w, h, idx, None);
        (f.left, f.top, f.interlaced) = (left, top, interlaced);
        e.write_frame(&f).unwrap();
    }
    b
}

/// A GIF stops at the next chunk of rows once the stop flag is set: it never decodes the
/// rest of the frame (it used to decode it whole, then look).
#[test]
fn a_gif_decode_stops_within_a_chunk_of_rows() {
    for interlaced in [false, true] {
        let d = folder(&format!("gif-stop-{interlaced}"));
        let p = d.join("big.gif");
        std::fs::write(&p, gif_file(1200, 900, (1200, 900, 0, 0), interlaced)).unwrap();
        let ((reached, wait), (go, gate)) = (channel(), channel());
        hook::GATES.lock().unwrap().push(hook::Gate { path: p.clone(), stage: "gif-chunk", reached, go: Some(gate) });
        let stop = Arc::new(AtomicBool::new(false));
        let (q, flag) = (p.clone(), stop.clone());
        let decode = std::thread::spawn(move || {
            hook::at(&q, "open");
            let f = BufReader::new(std::fs::File::open(&q).unwrap());
            gif(f, [300, 300], &flag).map(|d| d.is_some())
        });
        wait.recv_timeout(Duration::from_secs(5)).expect("the decode reached its first chunk");
        stop.store(true, Relaxed);
        go.send(()).unwrap();
        assert_eq!(decode.join().unwrap(), Ok(false), "interlaced {interlaced}: stopped");
        assert!(hook::seen(&p).contains(&"rows 0".to_string()), "{:?}", hook::seen(&p));
    }
}

/// Streamed or interlaced, a frame smaller than its canvas lands where it says, the rest
/// transparent, as the whole-frame decode put it.
#[test]
fn gif_frames_land_on_their_canvas() {
    let (cw, ch, frame) = (80, 60, (40, 30, 20, 10));
    let (fw, fh, fl, ft) = (frame.0 as usize, frame.1 as usize, frame.2 as usize, frame.3 as usize);
    let px = gradient(fw as u32, fh as u32);
    let mut want = vec![0u8; cw as usize * ch as usize * 4];
    for y in 0..fh {
        for x in 0..fw {
            let g = px[(y * fw + x) * 4];
            let at = ((ft + y) * cw as usize + fl + x) * 4;
            want[at..at + 4].copy_from_slice(&[g, g, g, 255]);
        }
    }
    for interlaced in [false, true] {
        let file = gif_file(cw, ch, frame, interlaced);
        let (pic, (w, h, rgba)) = gif(std::io::Cursor::new(&file), [1000, 1000], &never()).unwrap().unwrap();
        assert_eq!((pic.width, pic.height, w, h), (80, 60, 80, 60));
        assert!(rgba == want, "interlaced {interlaced}: pixels differ");
    }
}

/// WebP can't be stopped once it starts decoding, so it's held to 4 MP (the exception in
/// NORTHSTAR.md), lossy and lossless alike, checked from the header before decoding.
#[test]
fn webp_is_held_to_4_megapixels() {
    let chunk =
        |id: &[u8], body: &[u8]| [id, &(body.len() as u32).to_le_bytes(), body, if body.len() % 2 == 1 { &[0] } else { &[] }].concat();
    let riff = |body: Vec<u8>| [b"RIFF".as_slice(), &(body.len() as u32).to_le_bytes(), &body].concat();
    // A lossy header (a VP8 key frame) of `w` × `h`, with no image data after it.
    let lossy = |w: u16, h: u16| {
        let mut vp8 = vec![0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a];
        vp8.extend_from_slice(&w.to_le_bytes());
        vp8.extend_from_slice(&h.to_le_bytes());
        vp8.extend_from_slice(&[0; 64]);
        riff([b"WEBP".as_slice(), &chunk(b"VP8 ", &vp8)].concat())
    };
    // A lossless header (VP8L): 14-bit width and height, minus one.
    let lossless = |w: u32, h: u32| {
        let bits = (w - 1) | ((h - 1) << 14);
        let mut vp8l = vec![0x2f];
        vp8l.extend_from_slice(&bits.to_le_bytes());
        vp8l.extend_from_slice(&[0; 64]);
        riff([b"WEBP".as_slice(), &chunk(b"VP8L", &vp8l)].concat())
    };
    for (file, kind) in [(lossy(2400, 1700), "lossy"), (lossless(2400, 1700), "lossless")] {
        let err = image(Format::Webp, std::io::Cursor::new(&file), DEFAULT_FIT, &never()).unwrap_err();
        assert!(err.starts_with(TOO_LARGE) && err.contains("4.08 megapixels (the limit is 4 for WebP"), "{kind}: {err}");
    }
    // At the limit, it gets past the size check (to the decoder, which finds no data).
    for (file, kind) in [(lossy(2309, 1732), "lossy"), (lossless(2309, 1732), "lossless")] {
        let r = image(Format::Webp, std::io::Cursor::new(&file), DEFAULT_FIT, &never());
        assert!(r.as_ref().is_err_and(|e| !e.starts_with(TOO_LARGE)), "{kind}: {:?}", r.map(|_| ()));
    }
    // A real lossless one decodes.
    let (w, h) = (1400, 1300);
    let mut b = Vec::new();
    image_webp::WebPEncoder::new(&mut b).encode(&gradient(w, h), w, h, image_webp::ColorType::Rgba8).unwrap();
    assert!(image(Format::Webp, std::io::Cursor::new(b), DEFAULT_FIT, &never()).is_ok());
}

/// How long a decode goes on after it's told to stop: a 24 MP GIF (6000 × 4000) told 12 ms
/// in. `cargo test --release -- --ignored --nocapture gif_cancel_latency`
#[test]
#[ignore = "benchmark: encodes a 24 MP GIF"]
fn gif_cancel_latency() {
    for interlaced in [false, true] {
        let file = Arc::new(gif_file(6000, 4000, (6000, 4000, 0, 0), interlaced));
        let mut after: Vec<f64> = (0..7)
            .map(|_| {
                let stop = Arc::new(AtomicBool::new(false));
                let (f, flag) = (file.clone(), stop.clone());
                let t = std::thread::spawn(move || {
                    let r = gif(std::io::Cursor::new(&*f), [1000, 1000], &flag);
                    (r.map(|d| d.is_some()), Instant::now())
                });
                std::thread::sleep(Duration::from_millis(12));
                let told = Instant::now();
                stop.store(true, Relaxed);
                let (r, ended) = t.join().unwrap();
                assert_eq!(r, Ok(false));
                ended.saturating_duration_since(told).as_secs_f64() * 1000.0
            })
            .collect();
        after.sort_by(f64::total_cmp);
        let t = Instant::now();
        gif(std::io::Cursor::new(&*file), [1000, 1000], &never()).unwrap();
        let whole = t.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "24 MP GIF, interlaced {interlaced}: whole decode {whole:.0} ms; work after the stop: median {:.2} ms, max {:.2} ms",
            after[3], after[6]
        );
    }
}

fn gradient(w: u32, h: u32) -> Vec<u8> {
    (0..w * h).flat_map(|i| [(i % w * 255 / w) as u8, (i / w * 255 / h) as u8, 128, 255]).collect()
}

/// Writes one image in each format, `w` × `h`, into `d`.
pub(crate) fn write_images(d: &Path, w: u32, h: u32) -> Vec<(PathBuf, Format)> {
    use image::ImageEncoder;
    let px = gradient(w, h);
    let rgb: Vec<u8> = px.as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
    let mut out = Vec::new();
    let mut file = |name: &str, f: Format, bytes: Vec<u8>| {
        std::fs::write(d.join(name), bytes).unwrap();
        out.push((d.join(name), f));
    };
    let mut b = Vec::new();
    image::codecs::png::PngEncoder::new(&mut b).write_image(&px, w, h, image::ExtendedColorType::Rgba8).unwrap();
    file("a.png", Format::Png, b);
    let mut b = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut b, 90).write_image(&rgb, w, h, image::ExtendedColorType::Rgb8).unwrap();
    file("b.jpg", Format::Jpeg, b);
    let mut b = Vec::new();
    image::codecs::bmp::BmpEncoder::new(&mut b).write_image(&rgb, w, h, image::ExtendedColorType::Rgb8).unwrap();
    file("c.bmp", Format::Bmp, b);
    let mut b = Vec::new();
    image_webp::WebPEncoder::new(&mut b).encode(&px, w, h, image_webp::ColorType::Rgba8).unwrap();
    file("d.webp", Format::Webp, b);
    let mut b = Vec::new();
    {
        let palette: Vec<u8> = (0..=255u8).flat_map(|i| [i, i, i]).collect();
        let mut e = gif::Encoder::new(&mut b, w as u16, h as u16, &palette).unwrap();
        let idx: Vec<u8> = px.as_chunks::<4>().0.iter().map(|p| p[0]).collect();
        e.write_frame(&gif::Frame::from_indexed_pixels(w as u16, h as u16, idx, None)).unwrap();
    }
    file("e.gif", Format::Gif, b);
    out
}

#[test]
fn every_format_decodes_shrunk_to_the_panel() {
    let d = folder("formats");
    for (path, format) in write_images(&d, 640, 480) {
        let job = Job { path: path.clone(), is_dir: false, is_link: false, ext: String::new(), size: 1, fit: [200, 200] };
        let l = load(&job, &never()).unwrap();
        let Body::Image(p) = l.body else { panic!("{}: {:?}", path.display(), l.body) };
        assert_eq!((p.format, p.width, p.height, p.shown), (format, 640, 480, [200, 150]), "{}", path.display());
        let (w, h, px) = l.rgba.unwrap();
        assert_eq!(((w, h), px.len()), ((200, 150), 200 * 150 * 4));
        // The gradient survives: dark on the left, bright on the right.
        assert!(px[(75 * 200) * 4] < 20 && px[(75 * 200 + 199) * 4] > 230, "{}: {:?}", path.display(), &px[..8]);
    }
    // A small image keeps its size.
    let small = folder("formats-small");
    let (path, _) = write_images(&small, 30, 20).remove(1);
    let job = Job { path, is_dir: false, is_link: false, ext: String::new(), size: 1, fit: [200, 200] };
    let Body::Image(p) = load(&job, &never()).unwrap().body else { panic!() };
    assert_eq!(p.shown, [30, 20]);
}

/// A JPEG whose EXIF says "rotate 90°" shows turned: 40 × 20 stored, 20 × 40 shown.
#[test]
fn jpeg_exif_orientation_is_respected() {
    use image::ImageEncoder;
    let (w, h) = (40, 20);
    let rgb: Vec<u8> = gradient(w, h).as_chunks::<4>().0.iter().flat_map(|p| [p[0], p[1], p[2]]).collect();
    let mut jpg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpg).write_image(&rgb, w, h, image::ExtendedColorType::Rgb8).unwrap();
    // APP1: "Exif\0\0", a little-endian TIFF header, one IFD entry: Orientation (0x0112) = 6.
    let tiff = [
        b"II*\0".as_slice(),
        &8u32.to_le_bytes(),
        &1u16.to_le_bytes(),
        &[0x12, 0x01, 3, 0],
        &1u32.to_le_bytes(),
        &[6, 0, 0, 0],
        &0u32.to_le_bytes(),
    ]
    .concat();
    let body = [b"Exif\0\0".as_slice(), &tiff].concat();
    let app1 = [&[0xFF, 0xE1][..], &((body.len() + 2) as u16).to_be_bytes(), &body].concat();
    let bytes = [&jpg[..2], &app1, &jpg[2..]].concat();
    let (p, (dw, dh, _)) = image(Format::Jpeg, std::io::Cursor::new(bytes), [100, 100], &never()).unwrap().unwrap();
    assert_eq!((p.width, p.height, dw, dh), (20, 40, 20, 40));
}

#[test]
fn a_corrupt_image_says_so() {
    let d = folder("corrupt");
    let p = d.join("broken.png");
    std::fs::write(&p, b"\x89PNG\r\n\x1a\nthis is not a png at all").unwrap();
    let job = Job { path: p, is_dir: false, is_link: false, ext: "png".into(), size: 30, fit: DEFAULT_FIT };
    let Body::Error(why) = load(&job, &never()).unwrap().body else { panic!() };
    assert!(why.starts_with("Couldn't decode this PNG"), "{why}");
}

// ---- the panel in the app ---------------------------------------------------------------

fn key(s: &mut State, k: Key) -> bool {
    s.on_key(&KeyEvent { key: k, mods: Mods::default() }, &mut Tx::default())
}

fn space(s: &mut State) -> bool {
    key(s, Key::Text(" ".into()))
}

/// Polls as frames would until the preview's body lands (or 10 s pass).
fn settle(s: &mut State) {
    let until = Instant::now() + Duration::from_secs(10);
    while s.preview.as_ref().is_some_and(Preview::is_loading) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(2));
        s.poll(&mut Tx::default());
    }
    assert!(!s.preview.as_ref().is_some_and(Preview::is_loading), "never landed");
}

fn shown(s: &State) -> Option<&str> {
    s.preview.as_ref()?.target.as_ref().map(|t| t.name.as_str())
}

fn seen_open(p: &Path) -> usize {
    hook::seen(p).iter().filter(|s| *s == "open").count()
}

/// Closed, the preview does nothing at all: moving through files reads none of them.
#[test]
fn a_closed_preview_does_no_work() {
    let d = folder("closed");
    for n in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(d.join(n), "hello").unwrap();
    }
    let mut s = state_in(&d);
    for k in [Key::Down, Key::Down, Key::Up, Key::End, Key::Home] {
        key(&mut s, k);
        s.poll(&mut Tx::default());
    }
    s.run(Cmd::Reload, &mut Tx::default());
    crate::app::tests::settle_all(&mut s);
    assert!(s.preview.is_none());
    assert_eq!((s.preview_reads, s.preview_lands), (0, 0));
    for n in ["a.txt", "b.txt", "c.txt"] {
        assert!(hook::seen(&d.join(n)).is_empty(), "{n} was read");
    }
}

/// Space opens it on the selection, and it follows the selection; moving back reads again.
#[test]
fn space_opens_it_and_it_follows_the_selection() {
    let d = folder("follow");
    std::fs::write(d.join("a.txt"), "alpha\nbeta\n").unwrap();
    std::fs::create_dir(d.join("dir")).unwrap();
    for i in 0..3 {
        std::fs::write(d.join("dir").join(format!("f{i}")), "").unwrap();
    }
    let mut s = state_in(&d);
    assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some("dir"));
    assert!(space(&mut s));
    assert_eq!(shown(&s), Some("dir"), "the header shows at once");
    settle(&mut s);
    assert_eq!(s.preview.as_ref().unwrap().body, Body::Card(Card { items: Some(3), note: None }));
    key(&mut s, Key::Down);
    assert_eq!(shown(&s), Some("a.txt"));
    assert!(s.preview.as_ref().unwrap().is_loading(), "the list moved; the body follows");
    settle(&mut s);
    let Body::Text(t) = &s.preview.as_ref().unwrap().body else { panic!() };
    assert_eq!((t.line(0), t.line(1)), ("alpha", "beta"));
    // Back and forth: nothing is kept, so each visit reads again.
    key(&mut s, Key::Up);
    settle(&mut s);
    key(&mut s, Key::Down);
    settle(&mut s);
    assert_eq!(seen_open(&d.join("a.txt")), 2);
    assert!(space(&mut s));
    assert!(s.preview.is_none(), "Space again closes it");
}

/// While a filter has text, Space types a space (names have spaces); with nothing typed it
/// toggles the preview (a filter never starts with a space).
#[test]
fn space_types_into_a_filter() {
    let d = folder("filter-space");
    std::fs::write(d.join("my notes.txt"), "x").unwrap();
    let mut s = state_in(&d);
    key(&mut s, Key::Text("m".into()));
    key(&mut s, Key::Text("y".into()));
    assert!(space(&mut s));
    assert_eq!(s.cur().filter, "my ");
    assert!(s.preview.is_none());
    key(&mut s, Key::Text("n".into()));
    assert_eq!(s.cur().visible.len(), 1);
}

/// Esc: the palette first, then the preview, then the filter.
#[test]
fn escape_closes_the_palette_then_the_preview_then_clears_the_filter() {
    let d = folder("escape");
    std::fs::write(d.join("x1"), "x").unwrap();
    std::fs::write(d.join("x2"), "x").unwrap();
    let mut s = state_in(&d);
    key(&mut s, Key::Text("x".into()));
    s.run(Cmd::TogglePreview, &mut Tx::default());
    s.run(Cmd::Palette, &mut Tx::default());
    assert!(key(&mut s, Key::Escape));
    assert!(s.palette.is_none() && s.preview.is_some(), "the palette closes first");
    assert!(key(&mut s, Key::Escape));
    assert!(s.preview.is_none(), "then the preview");
    assert_eq!(s.cur().filter, "x", "the filter is still there");
    assert!(key(&mut s, Key::Escape));
    assert_eq!(s.cur().filter, "");
    // Toggle Preview needs a selection; Close Preview needs it open.
    assert_eq!(s.unavailable(Cmd::ClosePreview), Some("The preview isn't open"));
    let empty = folder("escape-empty");
    let s = state_in(&empty);
    assert_eq!(s.unavailable(Cmd::TogglePreview), Some("Select an item first"));
}

/// The selection moves while an image is being decoded: the decode stops (no rows decoded)
/// and its result never lands; the new selection's does.
#[test]
fn moving_on_during_a_decode_stops_it_and_drops_its_result() {
    let d = folder("cancel");
    let imgs = write_images(&d, 400, 300);
    let png = imgs[0].0.clone(); // a.png, then b.jpg
    let mut s = state_in(&d);
    assert_eq!(s.cur().selected().map(|e| e.name.as_str()), Some("a.png"));
    let ((reached, wait), (go, gate)) = (channel(), channel());
    hook::GATES.lock().unwrap().push(hook::Gate { path: png.clone(), stage: "header", reached, go: Some(gate) });
    space(&mut s);
    wait.recv_timeout(Duration::from_secs(5)).expect("the decode started");
    key(&mut s, Key::Down);
    assert_eq!(shown(&s), Some("b.jpg"));
    go.send(()).unwrap();
    settle(&mut s);
    let p = s.preview.as_ref().unwrap();
    assert!(matches!(&p.body, Body::Image(i) if i.format == Format::Jpeg), "{:?}", p.body);
    assert_eq!(s.preview_lands, 1, "only the JPEG landed");
    // Stopped at its next read, before decoding a single row.
    let seen = hook::seen(&png);
    assert!(seen.contains(&"header".to_string()) && !seen.contains(&"done".to_string()), "{seen:?}");
    assert!(!seen.iter().any(|s| s.starts_with("rows ") && s != "rows 0"), "{seen:?}");
}

/// Space tapped fast while a decode is held: reopening and moving on queue new requests,
/// each replacing the last, and none runs beside the held one; only the newest runs after
/// it. The decoder threads of earlier opens never pile up.
#[test]
fn reopening_never_runs_two_decodes_at_once() {
    let d = folder("reopen");
    let imgs = write_images(&d, 400, 300);
    let png = imgs[0].0.clone(); // a.png, then b.jpg, c.bmp, d.webp, e.gif
    let mut s = state_in(&d);
    let ((reached, wait), (go, gate)) = (channel(), channel());
    hook::GATES.lock().unwrap().push(hook::Gate { path: png.clone(), stage: "header", reached, go: Some(gate) });
    space(&mut s);
    wait.recv_timeout(Duration::from_secs(5)).expect("the decode started");
    for _ in 0..4 {
        space(&mut s); // close
        space(&mut s); // open again, on a.png
        key(&mut s, Key::Down); // b.jpg
        key(&mut s, Key::Up); // a.png
    }
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(hook::most_running(&d), 1, "a second read ran beside the held one");
    go.send(()).unwrap();
    settle(&mut s);
    let body = &s.preview.as_ref().unwrap().body;
    assert!(matches!(body, Body::Image(i) if i.format == Format::Png), "{body:?}");
    assert_eq!(hook::most_running(&d), 1);
    // Only the newest waiting request ran: the JPEG's were always replaced before they
    // started, and the PNG ran twice (the held read, then the newest request).
    assert_eq!(seen_open(&imgs[1].0), 0, "a replaced request ran");
    assert_eq!(seen_open(&png), 2);
}

/// GIFs whose canvas or first frame is zero pixels wide (34 bytes each): an error card, not
/// a panic in the worker and "Loading…" for good.
#[test]
fn a_gif_with_no_width_says_so() {
    let canvas: &[u8] = b"GIF89a\0\0\x01\0\x80\0\0\0\0\0\xff\xff\xff,\0\0\0\0\x01\0\x01\0\0\x02\x01L\0;";
    let frame: &[u8] = b"GIF89a\x01\0\x01\0\x80\0\0\0\0\0\xff\xff\xff,\0\0\0\0\0\0\x01\0\0\x02\x01L\0;";
    assert_eq!((canvas.len(), frame.len()), (34, 34));
    let d = folder("zero-gif");
    std::fs::write(d.join("a-canvas.gif"), canvas).unwrap();
    std::fs::write(d.join("b-frame.gif"), frame).unwrap();
    let mut s = state_in(&d);
    space(&mut s);
    settle(&mut s);
    let body = &s.preview.as_ref().unwrap().body;
    assert!(matches!(body, Body::Error(e) if e.contains("no pixels")), "{body:?}");
    key(&mut s, Key::Down);
    settle(&mut s);
    let body = &s.preview.as_ref().unwrap().body;
    assert!(matches!(body, Body::Error(e) if e.starts_with("Couldn't decode this GIF")), "{body:?}");
}

/// A decoder that panics: the panel says it couldn't, and the next preview still works.
#[test]
fn a_panicking_decoder_ends_in_an_error_not_loading() {
    let d = folder("panic");
    std::fs::write(d.join("a.txt"), "boom").unwrap();
    std::fs::write(d.join("b.txt"), "fine").unwrap();
    hook::PANICS.lock().unwrap().push(d.join("a.txt"));
    let mut s = state_in(&d);
    space(&mut s);
    settle(&mut s);
    let body = &s.preview.as_ref().unwrap().body;
    assert!(matches!(body, Body::Error(e) if e.contains("failed unexpectedly")), "{body:?}");
    key(&mut s, Key::Down);
    settle(&mut s);
    assert!(matches!(&s.preview.as_ref().unwrap().body, Body::Text(t) if t.line(0) == "fine"));
}

/// Closing during a read: it stops, and nothing lands afterwards.
#[test]
fn closing_during_a_read_drops_it() {
    let d = folder("close-read");
    std::fs::write(d.join("a.txt"), "hello").unwrap();
    let p = d.join("a.txt");
    let mut s = state_in(&d);
    let ((reached, wait), (go, gate)) = (channel(), channel());
    hook::GATES.lock().unwrap().push(hook::Gate { path: p.clone(), stage: "open", reached, go: Some(gate) });
    space(&mut s);
    wait.recv_timeout(Duration::from_secs(5)).expect("the read started");
    key(&mut s, Key::Escape);
    assert!(s.preview.is_none());
    go.send(()).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert!(!s.poll(&mut Tx::default()) && s.preview.is_none());
    assert_eq!(s.preview_lands, 0);
    assert!(!hook::seen(&p).contains(&"done".to_string()), "stopped before it finished");
}

#[cfg(unix)]
#[test]
fn a_link_previews_its_target_and_says_it_is_a_link() {
    let d = folder("link");
    std::fs::write(d.join("target.txt"), "through the link").unwrap();
    std::os::unix::fs::symlink(d.join("target.txt"), d.join("a-link")).unwrap();
    std::os::unix::fs::symlink(d.join("gone"), d.join("b-broken")).unwrap();
    let mut s = state_in(&d);
    space(&mut s);
    settle(&mut s);
    let p = s.preview.as_ref().unwrap();
    assert!(matches!(&p.body, Body::Text(t) if t.line(0) == "through the link"));
    assert!(p.link.as_deref().is_some_and(|l| l.starts_with("Link to ") && l.ends_with("target.txt")), "{:?}", p.link);
    key(&mut s, Key::Down);
    settle(&mut s);
    assert_eq!(s.preview.as_ref().unwrap().body, Body::Error("The link's target is missing".into()));
}

/// A FIFO would block its reader until something writes to it: it's never opened.
#[cfg(unix)]
#[test]
fn a_fifo_is_not_opened() {
    let d = folder("fifo");
    let p = d.join("pipe");
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    let job = Job { path: p, is_dir: false, is_link: false, ext: String::new(), size: 0, fit: DEFAULT_FIT };
    let Body::Card(c) = load(&job, &never()).unwrap().body else { panic!() };
    assert!(c.note.unwrap().starts_with("Not a regular file"));
}

/// The file is swapped for a FIFO between the check and the open: the open doesn't wait
/// for a writer, and the opened handle is found not to be a file.
#[cfg(unix)]
#[test]
fn a_fifo_swapped_in_after_the_check_is_not_read() {
    let d = folder("fifo-swap");
    let p = d.join("swap.txt");
    std::fs::write(&p, "a file, for now").unwrap();
    let ((reached, wait), (go, gate)) = (channel(), channel());
    hook::GATES.lock().unwrap().push(hook::Gate { path: p.clone(), stage: "before-open", reached, go: Some(gate) });
    let (tx, rx) = channel();
    let job = Job { path: p.clone(), is_dir: false, is_link: false, ext: "txt".into(), size: 15, fit: DEFAULT_FIT };
    std::thread::spawn(move || {
        let _ = tx.send(load(&job, &never()).map(|l| l.body));
    });
    wait.recv_timeout(Duration::from_secs(5)).expect("checked");
    std::fs::remove_file(&p).unwrap();
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    go.send(()).unwrap();
    let body = rx.recv_timeout(Duration::from_secs(2)).expect("the open blocked on the FIFO");
    assert!(matches!(&body, Some(Body::Card(c)) if c.note.as_deref().unwrap().starts_with("Not a regular file")), "{body:?}");
    // A regular file still reads normally (blocking reads back on).
    std::fs::remove_file(&p).unwrap();
    std::fs::write(&p, "a file again").unwrap();
    let job = Job { path: p, is_dir: false, is_link: false, ext: "txt".into(), size: 12, fit: DEFAULT_FIT };
    assert!(matches!(load(&job, &never()).unwrap().body, Body::Text(t) if t.line(0) == "a file again"));
}

#[test]
fn unreadable_and_platform_only_files_get_a_card() {
    let d = folder("cards");
    std::fs::write(d.join("a.pdf"), "%PDF-1.4").unwrap();
    std::fs::write(d.join("b.bin"), [0u8, 1, 2, 3, 0, 0, 0, 9]).unwrap();
    std::fs::write(d.join("c.txt"), "soon gone").unwrap();
    let mut s = state_in(&d);
    space(&mut s);
    settle(&mut s);
    let Body::Card(c) = &s.preview.as_ref().unwrap().body else { panic!() };
    // Linux has no thumbnailer Arbor uses, and says so; elsewhere the OS has nothing for an
    // eight-byte PDF, and the card says it can't preview it.
    if cfg!(any(target_os = "macos", windows)) {
        assert_eq!(c.note.as_deref(), Some(CANT));
    } else {
        assert!(c.note.as_deref().unwrap().starts_with("PDF: no preview on this system"), "{:?}", c.note);
    }
    assert!(hook::seen(&d.join("a.pdf")).contains(&"open".to_string()));
    key(&mut s, Key::Down);
    settle(&mut s);
    assert!(matches!(&s.preview.as_ref().unwrap().body, Body::Card(c) if c.note.as_deref().unwrap().starts_with("Binary file")));
    std::fs::remove_file(d.join("c.txt")).unwrap();
    key(&mut s, Key::Down);
    settle(&mut s);
    assert!(matches!(&s.preview.as_ref().unwrap().body, Body::Error(e) if e.starts_with("Couldn't read it")));
}

// ---- the OS's thumbnails --------------------------------------------------------------

/// Which files go to the OS's thumbnailer: formats Arbor doesn't decode, by extension, and
/// anything else whose bytes are neither text nor an image Arbor decodes. Text and Arbor's
/// own image formats never do.
#[test]
fn what_goes_to_the_os_thumbnailer() {
    for ext in
        ["pdf", "mp4", "mov", "mkv", "mp3", "m4a", "flac", "docx", "xlsx", "pages", "rtf", "epub", "ttf", "otf", "heic", "tiff", "dng"]
    {
        assert!(platform_kind(ext).is_some(), "{ext}");
    }
    // Their bytes decide: these are often text.
    for ext in ["png", "jpg", "gif", "webp", "bmp", "txt", "rs", "svg", "key", "ts", "json", ""] {
        assert!(platform_kind(ext).is_none(), "{ext}");
    }
    let d = folder("os-route");
    let imgs = write_images(&d, 64, 48); // a.png, b.jpg, c.bmp, d.webp, e.gif
    std::fs::write(d.join("f.pdf"), "%PDF-1.4 looks like text").unwrap();
    std::fs::write(d.join("g.blob"), [0u8, 159, 146, 150, 0, 0, 7, 1].repeat(64)).unwrap();
    std::fs::write(d.join("h.txt"), "plain text").unwrap();
    // Fakes everywhere, so a file that went to the OS can't hide.
    let all: Vec<PathBuf> = imgs.iter().map(|(p, _)| p.clone()).chain(["f.pdf", "g.blob", "h.txt"].map(|n| d.join(n))).collect();
    for p in &all {
        thumb::fake::set(p, thumb::fake::Fake::Image(30, 20));
    }
    let mut s = state_in(&d);
    space(&mut s);
    let mut got = Vec::new();
    for i in 0..all.len() {
        if i > 0 {
            key(&mut s, Key::Down);
        }
        settle(&mut s);
        let p = s.preview.as_ref().unwrap();
        let kind = match &p.body {
            Body::Image(i) if i.format == Format::Thumbnail => "os",
            Body::Image(_) => "decoded",
            Body::Text(_) => "text",
            b => panic!("{b:?}"),
        };
        got.push((p.target.as_ref().unwrap().name.clone(), kind));
    }
    let want = [("a.png", "decoded"), ("b.jpg", "decoded"), ("c.bmp", "decoded"), ("d.webp", "decoded"), ("e.gif", "decoded")]
        .into_iter()
        .chain([("f.pdf", "os"), ("g.blob", "os"), ("h.txt", "text")]);
    assert_eq!(got, want.map(|(n, k)| (n.to_string(), k)).collect::<Vec<_>>());
    for p in &all[..5] {
        assert!(!hook::seen(p).contains(&"os".to_string()), "{} went to the OS", p.display());
    }
    // The thumbnail lands as an image of its own size, with its pixels.
    key(&mut s, Key::Up);
    key(&mut s, Key::Up);
    settle(&mut s);
    let p = s.preview.as_ref().unwrap();
    assert_eq!(p.body, Body::Image(Picture { width: 30, height: 20, format: Format::Thumbnail, shown: [30, 20] }));
    assert_eq!(p.pixels.size(), (30, 20));
}

/// When the OS has nothing, the panel shows the card, never a blank panel: "Can't preview
/// this file" for a format it should know, and "Binary file" for anything else.
#[test]
fn the_card_shows_when_the_os_has_nothing() {
    let d = folder("os-nothing");
    std::fs::write(d.join("a.pdf"), "%PDF-1.4").unwrap();
    std::fs::write(d.join("b.blob"), [0u8, 1, 2, 3, 0, 0, 0, 9]).unwrap();
    thumb::fake::set(&d.join("a.pdf"), thumb::fake::Fake::Nothing);
    thumb::fake::set(&d.join("b.blob"), thumb::fake::Fake::Nothing);
    let mut s = state_in(&d);
    space(&mut s);
    settle(&mut s);
    assert_eq!(s.preview.as_ref().unwrap().body, Body::Card(Card { items: None, note: Some(CANT.into()) }));
    key(&mut s, Key::Down);
    settle(&mut s);
    let body = &s.preview.as_ref().unwrap().body;
    assert!(matches!(body, Body::Card(c) if c.note.as_deref().unwrap().starts_with("Binary file")), "{body:?}");
    assert!(hook::seen(&d.join("b.blob")).contains(&"os".to_string()));
}

/// An OS request that never answers is given up on after its limit: cancelled, and the card
/// shows ("Can't preview this file"), not "Loading…" for good.
#[test]
fn an_os_request_that_never_answers_is_given_up_on() {
    let d = folder("os-limit");
    std::fs::write(d.join("a.pdf"), "%PDF-1.4").unwrap();
    let os = Arc::new(thumb::fake::Os { limit: Some(Duration::from_millis(50)), ..Default::default() });
    thumb::fake::set(&d.join("a.pdf"), thumb::fake::Fake::Hang(os.clone()));
    let mut s = state_in(&d);
    let t = Instant::now();
    space(&mut s);
    settle(&mut s);
    assert!(t.elapsed() < Duration::from_secs(5));
    assert_eq!(s.preview.as_ref().unwrap().body, Body::Card(Card { items: None, note: Some(CANT.into()) }));
    assert_eq!(os.cancelled.load(Relaxed), 1);
}

/// The selection moves (or the panel closes) while the OS works on a thumbnail: the request
/// is cancelled at once, the next file's preview lands without waiting for it, and the OS's
/// late answer is dropped.
#[test]
fn moving_on_cancels_the_os_request() {
    let d = folder("os-cancel");
    std::fs::write(d.join("a.pdf"), "%PDF-1.4").unwrap();
    std::fs::write(d.join("b.txt"), "next").unwrap();
    let os = Arc::new(thumb::fake::Os::default());
    thumb::fake::set(&d.join("a.pdf"), thumb::fake::Fake::Hang(os.clone()));
    let mut s = state_in(&d);
    space(&mut s);
    let until = Instant::now() + Duration::from_secs(5);
    while os.started.load(Relaxed) == 0 {
        assert!(Instant::now() < until, "the OS request never started");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(s.preview.as_ref().unwrap().is_loading());
    key(&mut s, Key::Down);
    settle(&mut s);
    assert!(matches!(&s.preview.as_ref().unwrap().body, Body::Text(t) if t.line(0) == "next"));
    assert_eq!(os.cancelled.load(Relaxed), 1, "the OS request was cancelled");
    // The OS answers late: nothing lands.
    let done = os.done.lock().unwrap().take().unwrap();
    done(Some((2, 2, vec![255; 16])));
    std::thread::sleep(Duration::from_millis(20));
    s.poll(&mut Tx::default());
    assert!(matches!(&s.preview.as_ref().unwrap().body, Body::Text(_)));
    assert_eq!(s.preview_lands, 1, "only the text landed");
    // Closing cancels too.
    key(&mut s, Key::Up);
    let until = Instant::now() + Duration::from_secs(5);
    while os.started.load(Relaxed) < 2 {
        assert!(Instant::now() < until, "the second OS request never started");
        std::thread::sleep(Duration::from_millis(1));
    }
    key(&mut s, Key::Escape);
    assert!(s.preview.is_none());
    let until = Instant::now() + Duration::from_secs(5);
    while os.cancelled.load(Relaxed) < 2 {
        assert!(Instant::now() < until, "closing didn't cancel the OS request");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The panel follows the active pane: switching panes previews the other pane's selection.
#[test]
fn it_follows_the_active_pane() {
    let (a, b) = (folder("pane-a"), folder("pane-b"));
    std::fs::write(a.join("in-a.txt"), "a").unwrap();
    std::fs::write(b.join("in-b.txt"), "b").unwrap();
    let mut s = state_in(&a);
    s.run(Cmd::Split, &mut Tx::default());
    s.go_to(b.clone());
    crate::app::tests::settle_all(&mut s);
    space(&mut s);
    assert_eq!(shown(&s), Some("in-b.txt"));
    s.run(Cmd::OtherPane, &mut Tx::default());
    assert_eq!(shown(&s), Some("in-a.txt"));
    settle(&mut s);
}

/// How long a whole WebP decode takes (it can't be stopped once started, so this is the work
/// that goes on after a cancel), for each `.webp` in `ARBOR_WEBP_DIR`. Median of 5.
/// `ARBOR_WEBP_DIR=… cargo test --release -- --ignored --nocapture webp_decode_speed`
#[test]
#[ignore = "benchmark: needs ARBOR_WEBP_DIR"]
fn webp_decode_speed() {
    let Some(dir) = std::env::var_os("ARBOR_WEBP_DIR") else { return };
    let mut files: Vec<PathBuf> =
        std::fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "webp")).collect();
    files.sort();
    for p in files {
        let mut times: Vec<f64> = (0..5)
            .map(|_| {
                let t = Instant::now();
                let mut d = image_webp::WebPDecoder::new(BufReader::new(std::fs::File::open(&p).unwrap())).unwrap();
                let mut buf = vec![0; d.output_buffer_size().unwrap()];
                d.read_image(&mut buf).unwrap();
                t.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        times.sort_by(f64::total_cmp);
        // The whole preview path once (decode, then shrink to the panel), for its peak memory:
        // run with one file in the folder under `/usr/bin/time -v`.
        let r = image(Format::Webp, BufReader::new(std::fs::File::open(&p).unwrap()), DEFAULT_FIT, &never());
        assert!(r.is_ok(), "{:?}", r.err());
        let d = image_webp::WebPDecoder::new(BufReader::new(std::fs::File::open(&p).unwrap())).unwrap();
        let (w, h) = d.dimensions();
        let mp = (w as f64 * h as f64) / 1e6;
        eprintln!("{}: {w} × {h} ({mp:.1} MP): {:.1} ms, {:.2} ms/MP", p.file_name().unwrap().to_string_lossy(), times[2], times[2] / mp);
    }
}
