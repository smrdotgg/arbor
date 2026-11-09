//! Text: telling text from binary by its bytes, the encoding, and the lines the panel shows.

use super::*;

/// How a file's bytes read as text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    /// After a UTF-8 byte order mark.
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    /// Mostly UTF-8 with some invalid bytes (a broken file): shown with U+FFFD.
    Lossy,
    /// 8-bit text that isn't UTF-8 (Latin-1, Windows' own code page): read as Windows-1252,
    /// which is Latin-1 with printable characters where Latin-1 has control codes.
    Windows1252,
}

/// Whether `b` (a file's first bytes) is text, and how to read it. `cut` says the read stopped
/// at its limit before the end of the file, so its last character may be cut short; at the real
/// end of a file, trailing bytes that aren't UTF-8 count against it like any others. Not text: NUL bytes (beyond one in a thousand), or control characters (over 2%,
/// counting the five bytes Windows-1252 leaves undefined when it isn't UTF-8). Valid UTF-8 is
/// UTF-8; UTF-8 with a few invalid bytes (up to 10%) is shown with U+FFFD, but only if it has
/// real multi-byte characters too; anything else 8-bit is Windows-1252 (Latin-1 text has no
/// valid multi-byte sequences: "Café" is `43 61 66 E9`). A UTF-16 byte order mark reads as
/// UTF-16 (it's full of NULs).
pub fn sniff_text(b: &[u8], cut: bool) -> Option<Encoding> {
    if b.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Some(Encoding::Utf8Bom);
    }
    if b.starts_with(&[0xFF, 0xFE]) {
        return Some(Encoding::Utf16Le);
    }
    if b.starts_with(&[0xFE, 0xFF]) {
        return Some(Encoding::Utf16Be);
    }
    let nul = b.iter().filter(|&&c| c == 0).count();
    if nul * 1000 > b.len() {
        return None;
    }
    // Control characters other than tab, line breaks, form feed and escape.
    let ctrl = b.iter().filter(|&&c| (c < 0x20 && !matches!(c, b'\t' | b'\n' | b'\r' | 0x0C | 0x1B)) || c == 0x7F).count();
    if ctrl * 50 > b.len() {
        return None;
    }
    let (mut bad, mut multi) = (0, 0);
    for chunk in b.utf8_chunks() {
        multi += chunk.valid().chars().filter(|c| c.len_utf8() > 1).count();
        // An incomplete sequence at the very end of a read that stopped at its limit is a
        // character cut short, not a bad byte. ("Caf\xE9" at the end of a file is Latin-1.)
        let cut_short = cut && chunk.invalid().len() < 4 && chunk.invalid().as_ptr_range().end == b.as_ptr_range().end;
        if !chunk.invalid().is_empty() && !cut_short {
            bad += chunk.invalid().len();
        }
    }
    if bad == 0 {
        return Some(Encoding::Utf8);
    }
    if multi > 0 && bad * 10 <= b.len() {
        return Some(Encoding::Lossy);
    }
    let undefined = b.iter().filter(|&&c| matches!(c, 0x81 | 0x8D | 0x8F | 0x90 | 0x9D)).count();
    ((ctrl + undefined) * 50 <= b.len()).then_some(Encoding::Windows1252)
}

/// Windows-1252's characters for bytes 0x80–0x9F (where Latin-1 has control codes); its five
/// undefined bytes show as U+FFFD. 0xA0–0xFF are Latin-1's, the same code points.
pub(super) const CP1252_HIGH: [char; 32] = [
    '€', '\u{FFFD}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{FFFD}', 'Ž', '\u{FFFD}', '\u{FFFD}', '‘', '’', '“', '”',
    '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{FFFD}', 'ž', 'Ÿ',
];

pub(super) fn cp1252(b: u8) -> char {
    match b {
        0x80..=0x9F => CP1252_HIGH[(b - 0x80) as usize],
        b => b as char,
    }
}

/// Lays out the head of a text file for the panel: decoded, tabs expanded, control
/// characters shown as U+FFFD, lines over [`MAX_COLS`] cut. `total` is the file's size, for the
/// note when `truncated`.
pub fn text(bytes: &[u8], truncated: bool, total: u64) -> Option<Text> {
    let enc = sniff_text(bytes, truncated)?;
    let decoded: std::borrow::Cow<str> = match enc {
        Encoding::Utf8 => String::from_utf8_lossy(bytes),
        Encoding::Utf8Bom => String::from_utf8_lossy(&bytes[3..]),
        Encoding::Lossy => String::from_utf8_lossy(bytes),
        Encoding::Windows1252 => bytes.iter().map(|&b| cp1252(b)).collect::<String>().into(),
        Encoding::Utf16Le | Encoding::Utf16Be => {
            let units = bytes[2..].as_chunks::<2>().0.iter().map(|c| {
                let pair = [c[0], c[1]];
                if enc == Encoding::Utf16Le { u16::from_le_bytes(pair) } else { u16::from_be_bytes(pair) }
            });
            char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect::<String>().into()
        }
    };
    // A character cut by the bounded read shows as one U+FFFD at the end: drop it.
    let decoded = if truncated && enc == Encoding::Utf8 { decoded.trim_end_matches(char::REPLACEMENT_CHARACTER) } else { &decoded };
    let mut out = String::with_capacity(decoded.len() + decoded.len() / 16);
    let mut lines = Vec::new();
    let (mut cols, mut cut) = (0, false);
    let body = decoded.strip_suffix('\n').unwrap_or(decoded);
    if !body.is_empty() || decoded.is_empty() {
        for line in body.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let start = out.len() as u32;
            let mut col = 0;
            for c in line.chars() {
                if col >= MAX_COLS {
                    out.push('…');
                    cut = true;
                    col += 1;
                    break;
                }
                match c {
                    '\t' => {
                        let to = (col / TAB + 1) * TAB;
                        out.extend(std::iter::repeat_n(' ', to - col));
                        col = to;
                    }
                    c if c.is_control() => {
                        out.push(char::REPLACEMENT_CHARACTER);
                        col += 1;
                    }
                    c => {
                        out.push(c);
                        col += 1;
                    }
                }
            }
            cols = cols.max(col);
            lines.push(start..out.len() as u32);
        }
    }
    if decoded.is_empty() {
        lines.clear();
    }
    let mut notes = Vec::new();
    if truncated {
        notes.push(format!("Showing the first {} of {}", fs::fmt_size(TEXT_MAX as u64), fs::fmt_size(total)));
    }
    match enc {
        Encoding::Lossy => notes.push("Not valid UTF-8: invalid bytes show as �".into()),
        Encoding::Windows1252 => notes.push("Shown as Windows-1252".into()),
        Encoding::Utf16Le | Encoding::Utf16Be => notes.push("UTF-16".into()),
        _ => {}
    }
    if cut {
        notes.push(format!("Lines over {MAX_COLS} characters are cut"));
    }
    Some(Text { text: out, lines, cols, note: (!notes.is_empty()).then(|| notes.join(" · ")) })
}
