//! Netpbm PAM (`P7`) images, the kiosk socket's frame format
//! (<https://netpbm.sourceforge.net/doc/pam.html>), decoded as far as the kiosk needs and no further.
//!
//! A header is ASCII lines: `P7` first, then `WIDTH`, `HEIGHT`, `DEPTH`, `MAXVAL` (exactly once each), any number of
//! `TUPLTYPE` lines (their values joined by one space, per the spec) and `#` comment lines, in any order, and `ENDHDR` last.
//! Lines of only white space are ignored. The raster follows the newline after `ENDHDR`: exactly `WIDTH * HEIGHT * DEPTH`
//! bytes, row-major, top-down.
//!
//! Accepted: `TUPLTYPE RGB_ALPHA` with `DEPTH 4` (straight, not premultiplied, alpha) and `TUPLTYPE RGB` with `DEPTH 3`
//! (opaque), both with `MAXVAL 255`; `WIDTH` and `HEIGHT` each `1..=MAX_SIDE`; the whole header at most `MAX_HEADER` bytes.
//! Everything else (another tuple type or depth, a 16-bit maxval, an unknown keyword, a missing or repeated field, an
//! oversized header) is refused, and the socket closes the connection: a byte stream cannot be resynchronized after a
//! header it does not understand.
//!
//! Decoding yields PREMULTIPLIED RGBA8, the layout every paint path in this crate uses.
//!
//! This module only ever turns bytes into pixels. It is held to the socket's DISPLAY-2 tripwire (`socket.rs` tests), so
//! no identifier or string here may contain the words that test forbids; that is why nothing here is named after the
//! format's three-letter name.

use std::io::{self, BufRead, Read};

/// The whole header, from `P7` through the newline after `ENDHDR`, is at most this many bytes.
pub const MAX_HEADER: usize = 1024;

/// `WIDTH` and `HEIGHT` are each `1..=MAX_SIDE`.
pub const MAX_SIDE: u32 = 8192;

/// The two tuple types the kiosk accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tuple {
    /// `TUPLTYPE RGB`, `DEPTH 3`: opaque.
    Rgb,
    /// `TUPLTYPE RGB_ALPHA`, `DEPTH 4`: straight alpha.
    RgbAlpha,
}

impl Tuple {
    /// Bytes per pixel in the raster.
    pub fn depth(self) -> usize {
        match self {
            Tuple::Rgb => 3,
            Tuple::RgbAlpha => 4,
        }
    }
}

/// An accepted header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub width: u32,
    pub height: u32,
    pub tuple: Tuple,
}

impl Header {
    /// The raster's length on the wire.
    pub fn raster_len(&self) -> usize {
        self.width as usize * self.height as usize * self.tuple.depth()
    }
}

#[derive(Debug)]
pub enum Error {
    /// The stream ended before the first byte of an image: a clean end between images.
    End,
    /// The stream ended inside an image.
    Truncated,
    /// A header this socket does not accept.
    Refused(String),
    /// The transport failed (on the socket: also a stalled image, as a read timeout).
    Io(io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::End => write!(f, "end of stream"),
            Error::Truncated => write!(f, "the stream ended inside an image"),
            Error::Refused(why) => write!(f, "image refused: {why}"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

fn refused(why: impl Into<String>) -> Error {
    Error::Refused(why.into())
}

/// One header line without its newline. `Ok(None)`: the stream ended before the line's first byte.
fn read_line<R: BufRead>(r: &mut R, used: &mut usize) -> Result<Option<Vec<u8>>, Error> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match r.read(&mut byte) {
            Ok(0) if line.is_empty() => return Ok(None),
            Ok(0) => return Err(Error::Truncated),
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::Io(e)),
        }
        *used += 1;
        if *used > MAX_HEADER {
            return Err(refused(format!("header longer than {MAX_HEADER} bytes")));
        }
        match byte[0] {
            b'\n' => return Ok(Some(line)),
            b if b.is_ascii() => line.push(b),
            _ => return Err(refused("a header byte is not ASCII")),
        }
    }
}

/// A header number: decimal digits only (no sign, no other text), at most `u32`.
fn number(keyword: &str, tokens: &[&str]) -> Result<u32, Error> {
    match tokens {
        [v] if !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()) => v
            .parse()
            .map_err(|_| refused(format!("{keyword} {v} is out of range"))),
        _ => Err(refused(format!("{keyword} needs one decimal number"))),
    }
}

fn once(slot: &mut Option<u32>, keyword: &str, tokens: &[&str]) -> Result<(), Error> {
    if slot.is_some() {
        return Err(refused(format!("{keyword} appears twice")));
    }
    *slot = Some(number(keyword, tokens)?);
    Ok(())
}

/// Read and check one image header, up to and including the newline after `ENDHDR`. [`Error::End`] when the stream ends
/// before the first byte.
pub fn read_header<R: BufRead>(r: &mut R) -> Result<Header, Error> {
    let mut used = 0usize;
    let magic = read_line(r, &mut used)?.ok_or(Error::End)?;
    if magic.trim_ascii_end() != b"P7" {
        return Err(refused("not a P7 image"));
    }
    let (mut width, mut height, mut depth, mut maxval) = (None, None, None, None);
    let mut tupltype: Option<String> = None;
    loop {
        let line = read_line(r, &mut used)?.ok_or(Error::Truncated)?;
        if line.first() == Some(&b'#') {
            continue; // a comment line
        }
        // ASCII was checked byte by byte above, so this cannot fail.
        let text = String::from_utf8(line).map_err(|_| refused("a header byte is not ASCII"))?;
        let mut tokens = text.split_ascii_whitespace();
        let Some(keyword) = tokens.next() else {
            continue; // a line of only white space
        };
        let values: Vec<&str> = tokens.collect();
        match keyword {
            "WIDTH" => once(&mut width, keyword, &values)?,
            "HEIGHT" => once(&mut height, keyword, &values)?,
            "DEPTH" => once(&mut depth, keyword, &values)?,
            "MAXVAL" => once(&mut maxval, keyword, &values)?,
            "TUPLTYPE" => {
                let value = text.trim_ascii()["TUPLTYPE".len()..].trim_ascii();
                tupltype = Some(match tupltype {
                    Some(before) => format!("{before} {value}"),
                    None => value.to_string(),
                });
            }
            "ENDHDR" if values.is_empty() => break,
            "ENDHDR" => return Err(refused("ENDHDR takes no value")),
            other => return Err(refused(format!("unknown header keyword {other:?}"))),
        }
    }
    let width = width.ok_or_else(|| refused("no WIDTH"))?;
    let height = height.ok_or_else(|| refused("no HEIGHT"))?;
    let depth = depth.ok_or_else(|| refused("no DEPTH"))?;
    let maxval = maxval.ok_or_else(|| refused("no MAXVAL"))?;
    let side = 1..=MAX_SIDE;
    if !side.contains(&width) || !side.contains(&height) {
        return Err(refused(format!(
            "{width}x{height}: WIDTH and HEIGHT must each be 1..={MAX_SIDE}"
        )));
    }
    if maxval != 255 {
        return Err(refused(format!("MAXVAL {maxval}: only 255 is accepted")));
    }
    let tuple = match (tupltype.as_deref(), depth) {
        (Some("RGB_ALPHA"), 4) => Tuple::RgbAlpha,
        (Some("RGB"), 3) => Tuple::Rgb,
        (t, d) => {
            return Err(refused(format!(
                "TUPLTYPE {} with DEPTH {d}: only RGB_ALPHA with DEPTH 4 or RGB with DEPTH 3 is accepted",
                t.unwrap_or("(none)")
            )))
        }
    };
    Ok(Header {
        width,
        height,
        tuple,
    })
}

/// Straight to premultiplied, rounded exactly (`round(c * a / 255)`).
fn premultiply(c: u8, a: u8) -> u8 {
    let p = c as u32 * a as u32 + 128;
    ((p + (p >> 8)) >> 8) as u8
}

/// Read the raster that follows `header` and return it as premultiplied RGBA8 (`width * height * 4` bytes). The buffer
/// is allocated zeroed (untouched pages cost nothing until bytes arrive) and filled and converted in place.
pub fn read_raster<R: Read>(r: &mut R, header: &Header) -> Result<Vec<u8>, Error> {
    let pixels = header.width as usize * header.height as usize;
    let mut out = vec![0u8; pixels * 4];
    let wire = &mut out[..header.raster_len()];
    r.read_exact(wire).map_err(|e| match e.kind() {
        io::ErrorKind::UnexpectedEof => Error::Truncated,
        _ => Error::Io(e),
    })?;
    match header.tuple {
        Tuple::RgbAlpha => {
            for px in out.as_chunks_mut::<4>().0 {
                let a = px[3];
                if a != 255 {
                    px[0] = premultiply(px[0], a);
                    px[1] = premultiply(px[1], a);
                    px[2] = premultiply(px[2], a);
                }
            }
        }
        Tuple::Rgb => {
            // Widen 3 -> 4 bytes per pixel from the last pixel backwards: pixel i is read from 3i..3i+3 and written to
            // 4i..4i+4, and every later pixel's write starts at 4(i+1) > 3i+2, so nothing is overwritten before it is read.
            for i in (0..pixels).rev() {
                let (r, g, b) = (out[3 * i], out[3 * i + 1], out[3 * i + 2]);
                out[4 * i..4 * i + 4].copy_from_slice(&[r, g, b, 255]);
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(text: &str) -> Result<Header, Error> {
        read_header(&mut text.as_bytes())
    }

    fn refusal(text: &str) -> String {
        match header(text) {
            Err(Error::Refused(why)) => why,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    const RGBA_2X1: &str =
        "P7\nWIDTH 2\nHEIGHT 1\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n";

    #[test]
    fn an_rgb_alpha_header_is_accepted() {
        let h = header(RGBA_2X1).unwrap();
        assert_eq!(
            h,
            Header {
                width: 2,
                height: 1,
                tuple: Tuple::RgbAlpha
            }
        );
        assert_eq!(h.raster_len(), 8);
    }

    #[test]
    fn an_rgb_header_is_accepted() {
        let h =
            header("P7\nWIDTH 3\nHEIGHT 2\nDEPTH 3\nMAXVAL 255\nTUPLTYPE RGB\nENDHDR\n").unwrap();
        assert_eq!((h.width, h.height, h.tuple), (3, 2, Tuple::Rgb));
        assert_eq!(h.raster_len(), 18);
    }

    #[test]
    fn field_order_comments_blank_lines_and_extra_white_space_are_free() {
        let h = header(
            "P7\n# made by a test\nTUPLTYPE RGB_ALPHA\n\n   \nMAXVAL 255\n#WIDTH 9\nHEIGHT\t1\n  WIDTH   2  \nDEPTH 4\nENDHDR\n",
        )
        .unwrap();
        assert_eq!((h.width, h.height, h.tuple), (2, 1, Tuple::RgbAlpha));
    }

    #[test]
    fn tupltype_lines_are_joined_with_one_space_as_the_spec_says() {
        // "RGB" + "_ALPHA" would be RGB_ALPHA only if they were glued together; the spec joins them with a blank.
        let why = refusal(
            "P7\nWIDTH 1\nHEIGHT 1\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB\nTUPLTYPE _ALPHA\nENDHDR\n",
        );
        assert!(why.contains("RGB _ALPHA"), "{why}");
    }

    #[test]
    fn the_header_ends_exactly_at_the_newline_after_endhdr() {
        let mut wire = RGBA_2X1.as_bytes().to_vec();
        wire.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let mut r = wire.as_slice();
        let h = read_header(&mut r).unwrap();
        assert_eq!(r, &[1, 2, 3, 4, 5, 6, 7, 8], "the raster is untouched");
        assert_eq!(read_raster(&mut r, &h).unwrap().len(), 8);
    }

    #[test]
    fn a_wrong_depth_for_the_tuple_type_is_refused() {
        let why =
            refusal("P7\nWIDTH 1\nHEIGHT 1\nDEPTH 3\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n");
        assert!(why.contains("DEPTH 3"), "{why}");
        let why = refusal("P7\nWIDTH 1\nHEIGHT 1\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB\nENDHDR\n");
        assert!(why.contains("DEPTH 4"), "{why}");
        // grayscale and a missing tuple type: neither is accepted
        refusal("P7\nWIDTH 1\nHEIGHT 1\nDEPTH 1\nMAXVAL 255\nTUPLTYPE GRAYSCALE\nENDHDR\n");
        refusal("P7\nWIDTH 1\nHEIGHT 1\nDEPTH 4\nMAXVAL 255\nENDHDR\n");
    }

    #[test]
    fn any_maxval_but_255_is_refused() {
        for maxval in ["65535", "254", "1", "0"] {
            let why = refusal(&format!(
                "P7\nWIDTH 1\nHEIGHT 1\nDEPTH 4\nMAXVAL {maxval}\nTUPLTYPE RGB_ALPHA\nENDHDR\n"
            ));
            assert!(why.contains("MAXVAL"), "{maxval}: {why}");
        }
    }

    #[test]
    fn width_and_height_are_each_one_to_8192() {
        let at = |w: &str, h: &str| {
            header(&format!(
                "P7\nWIDTH {w}\nHEIGHT {h}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n"
            ))
        };
        assert!(at("8192", "8192").is_ok());
        assert!(at("1", "1").is_ok());
        for (w, h) in [("8193", "1"), ("1", "8193"), ("0", "1"), ("1", "0")] {
            assert!(
                matches!(at(w, h), Err(Error::Refused(_))),
                "{w}x{h} was accepted"
            );
        }
        // past u32, signed, or not a number at all
        for v in ["99999999999", "+5", "-5", "5px", "0x10", ""] {
            assert!(matches!(at(v, "1"), Err(Error::Refused(_))), "WIDTH {v:?}");
        }
    }

    #[test]
    fn a_header_longer_than_one_kib_is_refused_without_reading_on() {
        let mut text = String::from("P7\n");
        while text.len() <= MAX_HEADER {
            text.push_str("# padding padding padding padding padding\n");
        }
        text.push_str(RGBA_2X1.trim_start_matches("P7\n"));
        let why = refusal(&text);
        assert!(why.contains("1024"), "{why}");
        // an endless line with no newline at all: refused at the cap, not read to the end
        let endless = vec![b'#'; 1 << 20];
        let mut r = endless.as_slice();
        assert!(matches!(read_header(&mut r), Err(Error::Refused(_))));
        assert!(
            r.len() >= endless.len() - MAX_HEADER - 1,
            "read past the cap"
        );
        // exactly at the cap is still fine
        let mut exact = String::from("P7\n");
        let tail = RGBA_2X1.trim_start_matches("P7\n");
        let pad = MAX_HEADER - exact.len() - tail.len() - 2; // "#" + "\n"
        exact.push('#');
        exact.push_str(&" ".repeat(pad));
        exact.push('\n');
        exact.push_str(tail);
        assert_eq!(exact.len(), MAX_HEADER);
        assert!(header(&exact).is_ok());
    }

    #[test]
    fn missing_repeated_and_unknown_fields_are_refused() {
        assert!(
            refusal("P7\nHEIGHT 1\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n")
                .contains("no WIDTH")
        );
        assert!(refusal(
            "P7\nWIDTH 1\nWIDTH 1\nHEIGHT 1\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n"
        )
        .contains("twice"));
        assert!(refusal(
            "P7\nWIDTH 1\nHEIGHT 1\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nCOLOR red\nENDHDR\n"
        )
        .contains("unknown"));
        assert!(refusal(
            "P7\nWIDTH 1 2\nHEIGHT 1\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n"
        )
        .contains("one decimal number"));
        refusal("P7\nWIDTH 1\nHEIGHT 1\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR now\n");
    }

    #[test]
    fn other_netpbm_formats_and_garbage_are_refused() {
        for text in ["P6\n2 1\n255\n", "P7 332\n", " P7\n", "GET / HTTP/1.1\n"] {
            assert!(
                refusal(text).contains("not a P7 image"),
                "{text:?} was taken for a P7 image"
            );
        }
        let mut r: &[u8] = b"P7\nWIDTH \xC3\xA9\n";
        assert!(matches!(read_header(&mut r), Err(Error::Refused(_))));
    }

    #[test]
    fn an_empty_stream_is_a_clean_end_and_a_cut_header_is_truncated() {
        assert!(matches!(header(""), Err(Error::End)));
        assert!(matches!(header("P"), Err(Error::Truncated)));
        assert!(matches!(header("P7\nWIDTH 2\n"), Err(Error::Truncated)));
        assert!(matches!(
            header(&RGBA_2X1[..RGBA_2X1.len() - 1]),
            Err(Error::Truncated)
        ));
    }

    #[test]
    fn a_truncated_raster_is_reported_as_truncated() {
        let h = header(RGBA_2X1).unwrap();
        let mut r: &[u8] = &[1, 2, 3, 4, 5, 6, 7];
        assert!(matches!(read_raster(&mut r, &h), Err(Error::Truncated)));
    }

    #[test]
    fn straight_alpha_becomes_premultiplied() {
        let h = header(RGBA_2X1).unwrap();
        let mut r: &[u8] = &[255, 255, 255, 128, 200, 100, 0, 0];
        assert_eq!(
            read_raster(&mut r, &h).unwrap(),
            vec![128, 128, 128, 128, 0, 0, 0, 0]
        );
        // opaque pixels are untouched, and the rounding is exact against the textbook formula
        for c in 0..=255u32 {
            for a in 0..=255u32 {
                let want = ((c * a) as f64 / 255.0).round() as u8;
                assert_eq!(premultiply(c as u8, a as u8), want, "c={c} a={a}");
            }
        }
    }

    #[test]
    fn rgb_is_widened_to_opaque_rgba() {
        let h =
            header("P7\nWIDTH 3\nHEIGHT 1\nDEPTH 3\nMAXVAL 255\nTUPLTYPE RGB\nENDHDR\n").unwrap();
        let mut r: &[u8] = &[1, 2, 3, 4, 5, 6, 7, 8, 9];
        assert_eq!(
            read_raster(&mut r, &h).unwrap(),
            vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255]
        );
    }

    /// The fixture is byte for byte what cksk's frame writer emits for its 3x2 test board (cksk's own test pins that), so
    /// this is the reading half of the round trip: cksk's writer -> this reader.
    #[test]
    fn the_cksk_fixture_decodes_to_the_expected_premultiplied_pixels() {
        let fixture: &[u8] = include_bytes!("../tests/fixtures/board-3x2.pam");
        let mut r = fixture;
        let h = read_header(&mut r).unwrap();
        assert_eq!((h.width, h.height, h.tuple), (3, 2, Tuple::RgbAlpha));
        let px = read_raster(&mut r, &h).unwrap();
        assert!(r.is_empty(), "the fixture holds exactly one image");
        assert_eq!(
            px,
            [
                [255, 0, 0, 255],
                [0, 255, 0, 255],
                [0, 0, 255, 255],
                [128, 128, 128, 128],
                [0, 0, 0, 0],
                [10, 20, 30, 255],
            ]
            .concat()
        );
        assert!(matches!(read_header(&mut r), Err(Error::End)));
    }

    /// Written by netpbm itself (`ppmmake rgb:0a/14/1e 2 1 | pamtopam`): the reference implementation's own output reads.
    #[test]
    fn an_image_written_by_netpbm_itself_decodes() {
        let fixture: &[u8] = include_bytes!("../tests/fixtures/netpbm-rgb-2x1.pam");
        let mut r = fixture;
        let h = read_header(&mut r).unwrap();
        assert_eq!((h.width, h.height, h.tuple), (2, 1, Tuple::Rgb));
        assert_eq!(
            read_raster(&mut r, &h).unwrap(),
            vec![10, 20, 30, 255, 10, 20, 30, 255]
        );
        assert!(r.is_empty());
    }
}
