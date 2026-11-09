//! Images: sniffing the format, the size limits, and each decoder (PNG, JPEG, GIF, WebP, BMP),
//! shrinking to the panel's size as rows come and stopping between rows or chunks.

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
    Gif,
    Webp,
    Bmp,
    /// The OS's thumbnail of a file Arbor doesn't decode itself (its size is the thumbnail's).
    Thumbnail,
}

impl Format {
    pub fn name(self) -> &'static str {
        match self {
            Format::Png => "PNG",
            Format::Jpeg => "JPEG",
            Format::Gif => "GIF",
            Format::Webp => "WebP",
            Format::Bmp => "BMP",
            Format::Thumbnail => "preview",
        }
    }
}

/// An image format Arbor decodes, by its first bytes (not its name).
pub fn sniff_image(b: &[u8]) -> Option<Format> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Format::Png)
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(Format::Jpeg)
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some(Format::Gif)
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some(Format::Webp)
    } else if b.len() >= 26 && b.starts_with(b"BM") {
        Some(Format::Bmp)
    } else {
        None
    }
}

/// Why an image that says it's `w` × `h` can't be decoded when a side is zero (a corrupt
/// header): nothing to show, and a zero-wide row would break the shrinking.
pub fn no_pixels(w: u64, h: u64) -> Option<String> {
    (w == 0 || h == 0).then(|| format!("it says it's {w} × {h}, so it has no pixels"))
}

/// Why an image of `w` × `h` isn't decoded, when it's too big (checked from its header, before
/// anything is allocated for its pixels). `whole`: the decoder holds the whole image at once.
pub fn too_big(w: u64, h: u64, whole: bool) -> Option<String> {
    too_big_for(w, h, if whole { MAX_PIXELS_WHOLE } else { MAX_PIXELS }, "this format")
}

/// [`too_big`] with its own pixel limit, for `what` ("lossy WebP").
pub fn too_big_for(w: u64, h: u64, limit: u64, what: &str) -> Option<String> {
    if w > MAX_SIDE || h > MAX_SIDE {
        return Some(format!("{TOO_LARGE}: {w} × {h} is wider or taller than {MAX_SIDE} pixels"));
    }
    let mp = |n: u64| {
        let v = n as f64 / 1e6;
        if v < 10.0 { format!("{}", (v * 100.0).round() / 100.0) } else { format!("{v:.0}") }
    };
    (w.saturating_mul(h) > limit)
        .then(|| format!("{TOO_LARGE}: {w} × {h} is {} megapixels (the limit is {} for {what})", mp(w * h), mp(limit)))
}

/// The largest size at most `w` × `h` that fits in `fit` keeping the aspect ratio. Never
/// larger than the image itself.
pub fn fit_size(w: u32, h: u32, fit: [u32; 2]) -> (u32, u32) {
    let (bw, bh) = (fit[0].max(1), fit[1].max(1));
    if w <= bw && h <= bh {
        return (w.max(1), h.max(1));
    }
    let k = (bw as f64 / w as f64).min(bh as f64 / h as f64);
    (((w as f64 * k).round() as u32).clamp(1, bw), ((h as f64 * k).round() as u32).clamp(1, bh))
}

pub(super) type Decoded = (Picture, (u32, u32, Vec<u8>));

/// Decodes an image shrunk to fit `fit`. `Ok(None)` when stopped; `Err` says why it can't show.
pub(super) fn image<R: BufRead + Seek>(format: Format, r: R, fit: [u32; 2], stop: &AtomicBool) -> Result<Option<Decoded>, String> {
    let bad = |e: &dyn std::fmt::Display| {
        let e = e.to_string();
        if e.starts_with(TOO_LARGE) { e } else { format!("Couldn't decode this {}: {e}", format.name()) }
    };
    match format {
        Format::Png => png(r, fit, stop).map_err(|e| bad(&e)),
        Format::Jpeg => jpeg(r, fit, stop),
        Format::Gif => gif(r, fit, stop).map_err(|e| bad(&e)),
        Format::Webp => webp(r, fit, stop).map_err(|e| bad(&e)),
        Format::Bmp => bmp(r, fit, stop).map_err(|e| bad(&e)),
        // Never sniffed: only the OS makes these.
        Format::Thumbnail => Err("not an image format".into()),
    }
}

/// A source image's rows, shrunk to `dw` × `dh` as they arrive by averaging each output
/// pixel's box of source pixels (weighted by alpha, so transparent pixels don't darken edges).
/// A big image never needs to be in memory whole when its decoder gives rows.
pub struct Shrink {
    pub(super) sh: u32,
    pub(super) dw: u32,
    pub(super) dh: u32,
    /// The output column of each source column.
    pub(super) xmap: Vec<u32>,
    pub(super) acc: Vec<u64>,
    pub(super) n: Vec<u32>,
    /// The output row being gathered.
    pub(super) oy: u32,
    pub(super) out: Vec<u8>,
    pub(super) rgba: Vec<u8>,
}

impl Shrink {
    pub fn new(sw: u32, sh: u32, dw: u32, dh: u32) -> Shrink {
        let xmap = (0..sw).map(|x| (x as u64 * dw as u64 / sw as u64) as u32).collect();
        Shrink {
            sh,
            dw,
            dh,
            xmap,
            acc: vec![0; dw as usize * 4],
            n: vec![0; dw as usize],
            oy: 0,
            out: vec![0; dw as usize * dh as usize * 4],
            rgba: Vec::new(),
        }
    }

    /// Source row `y` (rows in order), `channels` bytes a pixel: gray, gray and alpha, RGB or RGBA.
    pub fn row(&mut self, y: u32, px: &[u8], channels: usize) {
        let oy = ((y as u64 * self.dh as u64) / self.sh as u64) as u32;
        if oy != self.oy {
            self.flush();
            self.oy = oy;
        }
        let mut rgba = std::mem::take(&mut self.rgba);
        to_rgba(px, channels, &mut rgba);
        for (p, &o) in rgba.as_chunks::<4>().0.iter().zip(&self.xmap) {
            let (o, a) = (o as usize, p[3] as u64);
            let acc = &mut self.acc[o * 4..o * 4 + 4];
            acc[0] += p[0] as u64 * a;
            acc[1] += p[1] as u64 * a;
            acc[2] += p[2] as u64 * a;
            acc[3] += a;
            self.n[o] += 1;
        }
        self.rgba = rgba;
    }

    pub(super) fn flush(&mut self) {
        let row = self.oy as usize * self.dw as usize * 4;
        if self.oy >= self.dh {
            return;
        }
        for x in 0..self.dw as usize {
            let (acc, n) = (&mut self.acc[x * 4..x * 4 + 4], self.n[x] as u64);
            if let Some(a) = acc[3].checked_div(n) {
                let out = &mut self.out[row + x * 4..row + x * 4 + 4];
                for c in 0..3 {
                    out[c] = acc[c].checked_div(acc[3]).unwrap_or(0).min(255) as u8;
                }
                out[3] = a as u8;
            }
            acc.fill(0);
            self.n[x] = 0;
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        self.flush();
        self.out
    }
}

/// One row of `channels`-byte pixels as RGBA.
pub(super) fn to_rgba(px: &[u8], channels: usize, out: &mut Vec<u8>) {
    out.clear();
    match channels {
        1 => out.extend(px.iter().flat_map(|&l| [l, l, l, 255])),
        2 => out.extend(px.as_chunks::<2>().0.iter().flat_map(|p| [p[0], p[0], p[0], p[1]])),
        3 => out.extend(px.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255])),
        _ => out.extend_from_slice(px),
    }
}

/// Shrinks a whole decoded image (`channels` bytes a pixel) to fit.
pub(super) fn shrink_whole(w: u32, h: u32, px: &[u8], channels: usize, fit: [u32; 2], stop: &AtomicBool) -> Option<(u32, u32, Vec<u8>)> {
    let (dw, dh) = fit_size(w, h, fit);
    let stride = w as usize * channels;
    // Every decoder refuses a zero side first (`no_pixels`); this is only so it can't panic.
    if stride == 0 || h == 0 {
        return Some((dw, dh, vec![0; dw as usize * dh as usize * 4]));
    }
    let mut s = Shrink::new(w, h, dw, dh);
    for (y, row) in px.chunks_exact(stride).take(h as usize).enumerate() {
        if y % 64 == 0 && stop.load(Relaxed) {
            return None;
        }
        s.row(y as u32, row, channels);
    }
    Some((dw, dh, s.finish()))
}

pub(super) fn picture(format: Format, w: u32, h: u32, rgba: (u32, u32, Vec<u8>)) -> Decoded {
    (Picture { width: w, height: h, format, shown: [rgba.0, rgba.1] }, rgba)
}

pub(super) fn png<R: BufRead + Seek>(mut r: R, fit: [u32; 2], stop: &AtomicBool) -> Result<Option<Decoded>, String> {
    // The size and interlacing from the header chunk, which always comes first, so a refusal
    // says why rather than the decoder's own limits error.
    if let Ok(head) = r.fill_buf()
        && head.len() >= 29
        && &head[12..16] == b"IHDR"
    {
        let be = |i: usize| u32::from_be_bytes([head[i], head[i + 1], head[i + 2], head[i + 3]]) as u64;
        if let Some(why) = no_pixels(be(16), be(20)).or_else(|| too_big(be(16), be(20), head[28] != 0)) {
            return Err(why);
        }
    }
    let mut d = png::Decoder::new_with_limits(r, png::Limits { bytes: MAX_PIXELS_WHOLE as usize * 4 });
    d.set_transformations(png::Transformations::normalize_to_color8());
    // The header only: nothing is allocated for pixels until the size is known to be fine.
    let mut reader = d.read_info().map_err(|e| e.to_string())?;
    let (w, h, interlaced) = {
        let i = reader.info();
        (i.width, i.height, i.interlaced)
    };
    if let Some(why) = no_pixels(w as u64, h as u64).or_else(|| too_big(w as u64, h as u64, interlaced)) {
        return Err(why);
    }
    let channels = reader.output_color_type().0.samples();
    if interlaced {
        // Adam7 passes don't arrive in row order: decode whole, then shrink.
        let mut buf = vec![0; reader.output_buffer_size().ok_or("too big")?];
        reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
        return Ok(shrink_whole(w, h, &buf, channels, fit, stop).map(|px| picture(Format::Png, w, h, px)));
    }
    let (dw, dh) = fit_size(w, h, fit);
    let mut s = Shrink::new(w, h, dw, dh);
    let mut y = 0;
    #[cfg(test)]
    let path = hook::current();
    while let Some(row) = reader.next_row().map_err(|e| e.to_string())? {
        if stop.load(Relaxed) {
            #[cfg(test)]
            hook::rows(&path, y);
            return Ok(None);
        }
        s.row(y, row.data(), channels);
        y += 1;
    }
    #[cfg(test)]
    hook::rows(&path, y);
    Ok(Some(picture(Format::Png, w, h, (dw, dh, s.finish()))))
}

pub(super) fn jpeg<R: BufRead + Seek>(mut r: R, fit: [u32; 2], stop: &AtomicBool) -> Result<Option<Decoded>, String> {
    let bad = |e: jpeg_decoder::Error| format!("Couldn't decode this JPEG: {e}");
    // Its markers first: the metadata's size, and the frame's size before the decoder
    // allocates anything for either.
    match jpeg_scan(&mut r) {
        Ok(Some((w, h, whole))) => {
            // A height of 0 is legal in a frame header (a DNL segment gives it later): the
            // decoder's own size is checked below.
            if let Some(why) = no_pixels(w as u64, h.max(1) as u64) {
                return Err(format!("Couldn't decode this JPEG: {why}"));
            }
            if let Some(why) = too_big(w as u64, h as u64, whole) {
                return Err(why);
            }
        }
        Ok(None) => {}
        Err(_) if stop.load(Relaxed) => return Ok(None),
        Err(e) => return Err(e),
    }
    r.seek(SeekFrom::Start(0)).map_err(|e| format!("Couldn't read it again from the start: {e}"))?;
    let mut d = jpeg_decoder::Decoder::new(r);
    d.read_info().map_err(bad)?;
    let info = d.info().ok_or("Couldn't decode this JPEG: no header")?;
    let (w, h) = (info.width as u32, info.height as u32);
    let progressive = info.coding_process != jpeg_decoder::CodingProcess::DctSequential;
    // Progressive JPEGs keep every coefficient of the whole image while decoding.
    if let Some(why) = no_pixels(w as u64, h as u64) {
        return Err(format!("Couldn't decode this JPEG: {why}"));
    }
    if let Some(why) = too_big(w as u64, h as u64, progressive || info.pixel_format == jpeg_decoder::PixelFormat::L16) {
        return Err(why);
    }
    let orientation = d.exif_data().and_then(image::metadata::Orientation::from_exif_chunk);
    let turned = matches!(
        orientation,
        Some(
            image::metadata::Orientation::Rotate90
                | image::metadata::Orientation::Rotate270
                | image::metadata::Orientation::Rotate90FlipH
                | image::metadata::Orientation::Rotate270FlipH
        )
    );
    // Shown turned, so it's the turned image that fits the panel.
    let want = if turned { [fit[1], fit[0]] } else { fit };
    let (want_w, want_h) = fit_size(w, h, want);
    // The IDCT does most of the shrinking (to the smallest of 1/8, 1/4, 1/2 or 1 that's still
    // at least the size wanted), the box filter the rest.
    let (sw, sh) = d.scale(want_w.min(u16::MAX as u32) as u16, want_h.min(u16::MAX as u32) as u16).map_err(bad)?;
    if stop.load(Relaxed) {
        return Ok(None);
    }
    let px = match d.decode() {
        Ok(px) => px,
        Err(_) if stop.load(Relaxed) => return Ok(None),
        Err(e) => return Err(bad(e)),
    };
    let fmt = d.info().map_or(info.pixel_format, |i| i.pixel_format);
    let (sw, sh) = (sw as u32, sh as u32);
    let (px, channels) = match fmt {
        jpeg_decoder::PixelFormat::L8 => (px, 1),
        jpeg_decoder::PixelFormat::RGB24 => (px, 3),
        // Big-endian 16-bit gray: its high bytes.
        jpeg_decoder::PixelFormat::L16 => (px.as_chunks::<2>().0.iter().map(|p| p[0]).collect(), 1),
        jpeg_decoder::PixelFormat::CMYK32 => (cmyk_to_rgb(&px), 3),
    };
    let Some((dw, dh, rgba)) = shrink_whole(sw, sh, &px, channels, want, stop) else { return Ok(None) };
    let (dw, dh, rgba) = match orientation {
        Some(o) if o != image::metadata::Orientation::NoTransforms => orient(dw, dh, rgba, o),
        _ => (dw, dh, rgba),
    };
    let (w, h) = if turned { (h, w) } else { (w, h) };
    Ok(Some(picture(Format::Jpeg, w, h, (dw, dh, rgba))))
}

/// Walks a JPEG's markers to its end (skipping the entropy-coded data between them): refuses
/// one with more than [`MAX_JPEG_META`] of metadata, and gives its first frame's size and
/// whether it's decoded whole (progressive, or 16-bit). Reads every byte once, never holding
/// more than a buffer. Anything it can't follow is left to the decoder to report.
pub(super) fn jpeg_scan<R: BufRead + Seek>(r: &mut R) -> Result<Option<(u32, u32, bool)>, String> {
    let io = |e: std::io::Error| format!("Couldn't read it: {e}");
    let mut two = [0u8; 2];
    r.read_exact(&mut two).map_err(io)?;
    if two != [0xFF, 0xD8] {
        return Ok(None);
    }
    let (mut meta, mut frame) = (0u64, None);
    let mut pending = None;
    loop {
        let m = match pending.take() {
            Some(m) => m,
            None => match jpeg_marker(r).map_err(io)? {
                Some(m) => m,
                None => break,
            },
        };
        match m {
            0xD9 => break,
            0x01 | 0xD0..=0xD8 => continue,
            _ => {}
        }
        if r.read_exact(&mut two).is_err() {
            break;
        }
        let mut body = (u16::from_be_bytes(two) as u64).saturating_sub(2);
        if matches!(m, 0xE0..=0xEF | 0xFE) {
            meta += body;
            if meta > MAX_JPEG_META {
                return Err(format!("{TOO_LARGE}: its metadata is over {} MB", MAX_JPEG_META >> 20));
            }
        }
        // A start of frame: precision, height, width.
        if matches!(m, 0xC0..=0xCF) && !matches!(m, 0xC4 | 0xC8 | 0xCC) && frame.is_none() && body >= 5 {
            let mut sof = [0u8; 5];
            r.read_exact(&mut sof).map_err(io)?;
            body -= 5;
            let (h, w) = (u16::from_be_bytes([sof[1], sof[2]]) as u32, u16::from_be_bytes([sof[3], sof[4]]) as u32);
            let whole = matches!(m, 0xC2 | 0xC6 | 0xCA | 0xCE) || sof[0] > 8;
            frame = Some((w, h, whole));
        }
        r.seek(SeekFrom::Current(body as i64)).map_err(io)?;
        if m == 0xDA {
            pending = jpeg_skip_scan(r).map_err(io)?;
        }
    }
    Ok(frame)
}

/// The next marker's code, after its 0xFF (and any fill bytes); `None` at the end or where
/// there's no marker.
pub(super) fn jpeg_marker<R: BufRead>(r: &mut R) -> std::io::Result<Option<u8>> {
    let mut b = [0u8; 1];
    if r.read(&mut b)? == 0 || b[0] != 0xFF {
        return Ok(None);
    }
    loop {
        if r.read(&mut b)? == 0 {
            return Ok(None);
        }
        if b[0] != 0xFF {
            return Ok(Some(b[0]));
        }
    }
}

/// Skips a scan's entropy-coded data: up to the next 0xFF that isn't a stuffed zero or a
/// restart marker. The marker's code, consumed.
pub(super) fn jpeg_skip_scan<R: BufRead>(r: &mut R) -> std::io::Result<Option<u8>> {
    let mut ff = false;
    loop {
        let (used, found) = {
            let buf = r.fill_buf()?;
            if buf.is_empty() {
                return Ok(None);
            }
            let mut i = 0;
            let mut found = None;
            while i < buf.len() {
                if ff {
                    match buf[i] {
                        0x00 | 0xD0..=0xD7 => ff = false,
                        0xFF => {}
                        m => {
                            found = Some(m);
                            i += 1;
                            break;
                        }
                    }
                    i += 1;
                } else {
                    match buf[i..].iter().position(|&b| b == 0xFF) {
                        Some(k) => {
                            i += k + 1;
                            ff = true;
                        }
                        None => i = buf.len(),
                    }
                }
            }
            (i, found)
        };
        r.consume(used);
        if found.is_some() {
            return Ok(found);
        }
    }
}

pub(super) fn cmyk_to_rgb(px: &[u8]) -> Vec<u8> {
    px.as_chunks::<4>()
        .0
        .iter()
        .flat_map(|p| {
            let k = p[3] as u32;
            [p[0], p[1], p[2]].map(|c| (c as u32 * k / 255) as u8)
        })
        .collect()
}

/// Applies an EXIF orientation to RGBA pixels.
pub(super) fn orient(w: u32, h: u32, rgba: Vec<u8>, o: image::metadata::Orientation) -> (u32, u32, Vec<u8>) {
    let Some(img) = image::RgbaImage::from_raw(w, h, rgba) else { return (w, h, Vec::new()) };
    let mut img = image::DynamicImage::ImageRgba8(img);
    img.apply_orientation(o);
    let img = img.into_rgba8();
    (img.width(), img.height(), img.into_raw())
}

/// Frame rows a GIF decodes between looks at the stop flag: a few hundred microseconds of LZW
/// for a 6000-pixel row.
pub(super) const GIF_ROWS: usize = 16;

/// A GIF's first frame, on its canvas, decoded `GIF_ROWS` rows at a time with a look at `stop`
/// between them, so moving on stops it within a chunk. Rows in order (most GIFs) stream into the
/// shrinker, so neither the frame nor the canvas is ever held whole; an interlaced frame's rows
/// come in four passes, so it fills a frame buffer first (still a chunk at a time), then shrinks.
pub(super) fn gif<R: BufRead + Seek>(r: R, fit: [u32; 2], stop: &AtomicBool) -> Result<Option<Decoded>, String> {
    let mut opts = gif::DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::RGBA);
    if let Some(limit) = std::num::NonZeroU64::new(MAX_PIXELS_WHOLE * 4) {
        opts.set_memory_limit(gif::MemoryLimit::Bytes(limit));
    }
    let mut d = opts.read_info(r).map_err(|e| e.to_string())?;
    let (w, h) = (d.width() as u32, d.height() as u32);
    if let Some(why) = no_pixels(w as u64, h as u64).or_else(|| too_big(w as u64, h as u64, false)) {
        return Err(why);
    }
    let Some(frame) = d.next_frame_info().map_err(|e| e.to_string())? else { return Err("it has no frames".into()) };
    let (fw, fh, fl, ft, interlaced) =
        (frame.width as usize, frame.height as usize, frame.left as usize, frame.top as usize, frame.interlaced);
    if let Some(why) = no_pixels(fw as u64, fh as u64) {
        return Err(format!("its first frame {why}"));
    }
    if interlaced && let Some(why) = too_big(w as u64, h as u64, true) {
        return Err(why);
    }
    #[cfg(test)]
    let path = hook::current();
    let row_bytes = fw * 4;
    let mut chunk = vec![0u8; row_bytes * GIF_ROWS.min(fh)];
    // Reads the frame's next `n` rows into `chunk`; `None` when stopped first.
    let next = |d: &mut gif::Decoder<R>, chunk: &mut [u8], done: usize| -> Result<Option<()>, String> {
        #[cfg(not(test))]
        let _ = done; // rows decoded so far: tests count them
        #[cfg(test)]
        hook::at(&path, "gif-chunk");
        if stop.load(Relaxed) {
            #[cfg(test)]
            hook::rows(&path, done as u32);
            return Ok(None);
        }
        if !d.fill_buffer(chunk).map_err(|e| e.to_string())? {
            return Err("its pixel data ends early".into());
        }
        Ok(Some(()))
    };
    if interlaced {
        // Pass by pass, each row where it goes.
        let rows: Vec<usize> = [(0, 8), (4, 8), (2, 4), (1, 2)].into_iter().flat_map(|(from, step)| (from..fh).step_by(step)).collect();
        let mut frame_px = vec![0u8; row_bytes * fh];
        for (k, ys) in rows.chunks(GIF_ROWS).enumerate() {
            let buf = &mut chunk[..ys.len() * row_bytes];
            if next(&mut d, buf, k * GIF_ROWS)?.is_none() {
                return Ok(None);
            }
            for (i, &y) in ys.iter().enumerate() {
                frame_px[y * row_bytes..(y + 1) * row_bytes].copy_from_slice(&buf[i * row_bytes..(i + 1) * row_bytes]);
            }
        }
        let canvas = place(&frame_px, (fw, fh, fl, ft), (w as usize, h as usize));
        return Ok(shrink_whole(w, h, &canvas, 4, fit, stop).map(|px| picture(Format::Gif, w, h, px)));
    }
    let (dw, dh) = fit_size(w, h, fit);
    let mut s = Shrink::new(w, h, dw, dh);
    // The canvas row being shrunk: transparent, with the frame's row in its place.
    let (cw, empty) = (w as usize * 4, vec![0u8; w as usize * 4]);
    let mut line = vec![0u8; cw];
    let (x0, n) = ((fl * 4).min(cw), (row_bytes).min(cw.saturating_sub(fl * 4)));
    let mut y = 0usize;
    for cy in 0..h as usize {
        if cy < ft || cy >= ft + fh {
            s.row(cy as u32, &empty, 4);
            continue;
        }
        let i = (cy - ft) % GIF_ROWS;
        if i == 0 {
            let rows = GIF_ROWS.min(fh - (cy - ft));
            if next(&mut d, &mut chunk[..rows * row_bytes], y)?.is_none() {
                return Ok(None);
            }
        }
        line[x0..x0 + n].copy_from_slice(&chunk[i * row_bytes..i * row_bytes + n]);
        s.row(cy as u32, &line, 4);
        y += 1;
    }
    #[cfg(test)]
    hook::rows(&path, y as u32);
    Ok(Some(picture(Format::Gif, w, h, (dw, dh, s.finish()))))
}

/// A frame of `fw` × `fh` RGBA pixels at (`fl`, `ft`) on a transparent canvas of `w` × `h`.
pub(super) fn place(frame: &[u8], (fw, fh, fl, ft): (usize, usize, usize, usize), (w, h): (usize, usize)) -> Vec<u8> {
    let mut canvas = vec![0u8; w * h * 4];
    for (y, row) in frame.chunks_exact(fw * 4).take(fh).enumerate() {
        let cy = ft + y;
        if cy >= h || fl >= w {
            continue;
        }
        let n = fw.min(w - fl) * 4;
        let at = (cy * w + fl) * 4;
        canvas[at..at + n].copy_from_slice(&row[..n]);
    }
    canvas
}

pub(super) fn webp<R: BufRead + Seek>(mut r: R, fit: [u32; 2], stop: &AtomicBool) -> Result<Option<Decoded>, String> {
    // The sizes in its bitstreams, before the decoder: its canvas size alone can hide a bigger
    // image inside, which would be decoded before it's refused.
    let frames = webp_frames(&mut r).map_err(|e| e.to_string())?;
    r.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let mut d = image_webp::WebPDecoder::new(r).map_err(|e| e.to_string())?;
    let (w, h) = d.dimensions();
    let (limit, what) = (MAX_PIXELS_WEBP, "WebP, which can't be stopped once it starts decoding");
    if let Some(why) = no_pixels(w as u64, h as u64).or_else(|| too_big_for(w as u64, h as u64, limit, what)) {
        return Err(why);
    }
    for (fw, fh) in frames {
        if let Some(why) = too_big_for(fw as u64, fh as u64, limit, what) {
            return Err(why);
        }
        if fw > w || fh > h {
            return Err(format!("its image ({fw} × {fh}) is bigger than its canvas ({w} × {h})"));
        }
    }
    let mut buf = vec![0; d.output_buffer_size().ok_or("too big")?];
    d.read_image(&mut buf).map_err(|e| e.to_string())?;
    let channels = if d.has_alpha() { 4 } else { 3 };
    Ok(shrink_whole(w, h, &buf, channels, fit, stop).map(|px| picture(Format::Webp, w, h, px)))
}

/// The width and height each image bitstream in a WebP file says it has (`VP8 `, `VP8L`, and
/// those inside animation frames), read from their headers without decoding. At most
/// [`WEBP_CHUNKS`] chunks are looked at.
pub(super) fn webp_frames<R: Read + Seek>(r: &mut R) -> std::io::Result<Vec<(u32, u32)>> {
    let mut head = [0u8; 12];
    r.read_exact(&mut head)?;
    let end = (u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as u64).saturating_add(8);
    let mut out = Vec::new();
    webp_chunks(r, 12, end, &mut out, 0)?;
    Ok(out)
}

pub(super) const WEBP_CHUNKS: usize = 100_000;

pub(super) fn webp_chunks<R: Read + Seek>(r: &mut R, mut at: u64, end: u64, out: &mut Vec<(u32, u32)>, depth: u8) -> std::io::Result<()> {
    let mut seen = 0;
    while at + 8 <= end && seen < WEBP_CHUNKS {
        seen += 1;
        r.seek(SeekFrom::Start(at))?;
        let mut h = [0u8; 8];
        if r.read_exact(&mut h).is_err() {
            break;
        }
        let size = u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as u64;
        let body = at + 8;
        match &h[..4] {
            b"VP8 " => {
                // Frame tag (3 bytes), start code 9d 01 2a, then 14-bit width and height.
                let mut b = [0u8; 10];
                if r.read_exact(&mut b).is_ok() && b[3..6] == [0x9d, 0x01, 0x2a] {
                    let w = u16::from_le_bytes([b[6], b[7]]) as u32 & 0x3fff;
                    let h = u16::from_le_bytes([b[8], b[9]]) as u32 & 0x3fff;
                    out.push((w, h));
                }
            }
            b"VP8L" => {
                // Signature 0x2f, then 14 bits of width − 1 and 14 of height − 1.
                let mut b = [0u8; 5];
                if r.read_exact(&mut b).is_ok() && b[0] == 0x2f {
                    let v = u32::from_le_bytes([b[1], b[2], b[3], b[4]]);
                    out.push(((v & 0x3fff) + 1, ((v >> 14) & 0x3fff) + 1));
                }
            }
            // An animation frame: 16 bytes of its own header, then its chunks.
            b"ANMF" if depth == 0 => webp_chunks(r, body + 16, (body + size).min(end), out, 1)?,
            _ => {}
        }
        at = body + size + (size & 1);
    }
    Ok(())
}

pub(super) fn bmp<R: BufRead + Seek>(r: R, fit: [u32; 2], stop: &AtomicBool) -> Result<Option<Decoded>, String> {
    use image::ImageDecoder;
    let d = image::codecs::bmp::BmpDecoder::new(r).map_err(|e| e.to_string())?;
    let (w, h) = d.dimensions();
    if let Some(why) = no_pixels(w as u64, h as u64).or_else(|| too_big(w as u64, h as u64, true)) {
        return Err(why);
    }
    let channels = d.color_type().channel_count() as usize;
    if d.color_type().bytes_per_pixel() as usize != channels {
        return Err("16-bit channels aren't supported".into());
    }
    let mut buf = vec![0; usize::try_from(d.total_bytes()).map_err(|e| e.to_string())?];
    d.read_image(&mut buf).map_err(|e| e.to_string())?;
    Ok(shrink_whole(w, h, &buf, channels, fit, stop).map(|px| picture(Format::Bmp, w, h, px)))
}
