//! The platform a golden PNG was recorded on, kept in the PNG itself: a `tEXt` chunk with the
//! keyword [`KEYWORD`] and the platform key (`macos-aarch64`) as its text.
//!
//! Decoders ignore text chunks, so a stamped golden shows the same pixels everywhere and stays one
//! file next to the tests. Tools that strip ancillary chunks (PNG optimizers) drop the origin with
//! them; a golden without one is read as recorded on the `fallback_platform` of the workspace.

use std::{borrow::Cow, ops::Range};

use crate::platform::PlatformKey;

/// The keyword of the origin's `tEXt` chunk.
pub const KEYWORD: &str = "gleon:platform";

/// The PNG file signature.
const SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// One chunk of a PNG: its type, its data, and where its bytes (length, type, data, CRC) are.
struct Chunk<'a> {
    kind: [u8; 4],
    data: &'a [u8],
    range: Range<usize>,
}

impl Chunk<'_> {
    /// The text of an origin chunk ([`KEYWORD`]), whatever it names.
    fn origin_text(&self) -> Option<&[u8]> {
        (self.kind == *b"tEXt")
            .then(|| {
                self.data
                    .strip_prefix(KEYWORD.as_bytes())?
                    .strip_prefix(&[0])
            })
            .flatten()
    }

    /// The platform an origin chunk names; `None` for any other chunk, or a text that is no
    /// platform key.
    fn origin(&self) -> Option<PlatformKey> {
        PlatformKey::parse(std::str::from_utf8(self.origin_text()?).ok()?).ok()
    }
}

/// The chunks of a PNG in order, read lazily after its signature: they end at the end of the
/// file, or before the first that does not fit in it (the CRCs are not checked: the decoder does
/// that).
struct Chunks<'a> {
    png: &'a [u8],
    at: usize,
}

impl<'a> Iterator for Chunks<'a> {
    type Item = Chunk<'a>;

    fn next(&mut self) -> Option<Chunk<'a>> {
        let rest = self.png.get(self.at..)?;
        let length = usize::try_from(u32::from_be_bytes(rest.get(..4)?.try_into().ok()?)).ok()?;
        let end = length.checked_add(12)?;
        let bytes = rest.get(..end)?;
        let chunk = Chunk {
            kind: bytes[4..8].try_into().ok()?,
            data: &bytes[8..8 + length],
            range: self.at..self.at + end,
        };
        self.at += end;
        Some(chunk)
    }
}

/// The chunks of `png`, `None` for no PNG.
fn chunks(png: &[u8]) -> Option<Chunks<'_>> {
    png.starts_with(SIGNATURE).then_some(Chunks {
        png,
        at: SIGNATURE.len(),
    })
}

/// The origin chunk of a PNG ([`locate`]): the platform it names, and where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    /// The platform the PNG was recorded on.
    pub platform: PlatformKey,
    range: Range<usize>,
}

impl Stamp {
    /// Whether `image` is `png` (the PNG this stamp was located in) without this stamp, compared
    /// in place.
    #[must_use]
    pub fn is_stripped(&self, png: &[u8], image: &[u8]) -> bool {
        let Range { start, end } = self.range;
        image.len() + (end - start) == png.len()
            && image[..start] == png[..start]
            && image[start..] == png[end..]
    }
}

/// The first origin chunk of `png` that names a platform; `None` without one (or for no PNG).
#[must_use]
pub fn locate(png: &[u8]) -> Option<Stamp> {
    chunks(png)?.find_map(|chunk| {
        Some(Stamp {
            platform: chunk.origin()?,
            range: chunk.range,
        })
    })
}

/// The platform `png` was recorded on: what its origin chunk names, `None` without one (or for
/// no PNG, or a text that is no platform key).
#[must_use]
pub fn read(png: &[u8]) -> Option<PlatformKey> {
    locate(png).map(|stamp| stamp.platform)
}

/// `png` recorded on `platform`: one origin chunk right after the header (any earlier origin
/// chunk dropped, whatever it names), everything else unchanged. `None` for no PNG, or one whose
/// chunks do not fill it from its header on.
#[must_use]
pub fn stamp(png: &[u8], platform: &PlatformKey) -> Option<Vec<u8>> {
    let mut chunks = chunks(png)?;
    let header = chunks.next().filter(|first| first.kind == *b"IHDR")?;
    let mut data = KEYWORD.as_bytes().to_vec();
    data.push(0);
    data.extend_from_slice(platform.as_str().as_bytes());
    let mut crc = crc32fast::Hasher::new();
    crc.update(b"tEXt");
    crc.update(&data);
    let mut out = Vec::with_capacity(png.len() + data.len() + 12);
    out.extend_from_slice(&png[..header.range.end]);
    out.extend_from_slice(&u32::try_from(data.len()).ok()?.to_be_bytes());
    out.extend_from_slice(b"tEXt");
    out.extend_from_slice(&data);
    out.extend_from_slice(&crc.finalize().to_be_bytes());
    for chunk in chunks.by_ref() {
        if chunk.origin_text().is_none() {
            out.extend_from_slice(&png[chunk.range]);
        }
    }
    (chunks.at == png.len()).then_some(out)
}

/// `png` without its origin chunk: the image as its encoder wrote it. Borrowed when it has none
/// that names a platform (or is no PNG).
#[must_use]
pub fn strip(png: &[u8]) -> Cow<'_, [u8]> {
    match locate(png) {
        Some(Stamp { range, .. }) => Cow::Owned([&png[..range.start], &png[range.end..]].concat()),
        None => Cow::Borrowed(png),
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::missing_panics_doc,
    clippy::pedantic,
    clippy::nursery,
    reason = "test code: panics are assertions, and pedantic/nursery style lints are not enforced in tests"
)]
mod tests {
    use super::*;
    use crate::compare::encode_png;

    fn png() -> Vec<u8> {
        let mut image = image::RgbaImage::from_pixel(3, 2, image::Rgba([10, 20, 30, 255]));
        image.put_pixel(1, 1, image::Rgba([200, 0, 0, 128]));
        encode_png(&image).unwrap()
    }

    fn key(key: &str) -> PlatformKey {
        PlatformKey::parse(key).unwrap()
    }

    fn pixels(png: &[u8]) -> image::RgbaImage {
        image::load_from_memory(png).unwrap().to_rgba8()
    }

    #[test]
    fn test_a_stamp_is_read_back_and_invisible_to_decoders() {
        let png = png();
        assert_eq!(read(&png), None);
        assert!(matches!(strip(&png), Cow::Borrowed(_)));

        let linux = stamp(&png, &key("linux-x86_64")).unwrap();
        assert_eq!(read(&linux), Some(key("linux-x86_64")));
        assert_eq!(pixels(&linux), pixels(&png));
        assert_eq!(strip(&linux), png);
        // Right after the header: IHDR is 8 + 25 bytes into the file.
        assert_eq!(&linux[37..41], b"tEXt");
        let located = locate(&linux).unwrap();
        assert_eq!(located.platform, key("linux-x86_64"));
        assert!(located.is_stripped(&linux, &png));
        let mut other = png.clone();
        *other.last_mut().unwrap() ^= 1;
        assert!(!located.is_stripped(&linux, &other));
        assert!(!located.is_stripped(&linux, &png[1..]));

        // Stamped again: one origin chunk, the new platform, the same bytes as a fresh stamp.
        let macos = stamp(&linux, &key("macos-aarch64")).unwrap();
        assert_eq!(read(&macos), Some(key("macos-aarch64")));
        assert_eq!(macos, stamp(&png, &key("macos-aarch64")).unwrap());
        assert_eq!(strip(&macos), png);
    }

    #[test]
    fn test_other_chunks_stay() {
        let png = png();
        let body = b"tEXtSoftware\0another encoder";
        let mut other = png[..33].to_vec();
        other.extend_from_slice(&u32::try_from(body.len() - 4).unwrap().to_be_bytes());
        other.extend_from_slice(body);
        other.extend_from_slice(&crc32fast::hash(body).to_be_bytes());
        other.extend_from_slice(&png[33..]);

        assert_eq!(read(&other), None, "another text chunk is no origin");
        let stamped = stamp(&other, &key("windows-x86_64")).unwrap();
        assert_eq!(read(&stamped), Some(key("windows-x86_64")));
        assert_eq!(strip(&stamped), other);
        assert_eq!(pixels(&stamped), pixels(&png));
    }

    #[test]
    fn test_no_png_and_no_platform_key() {
        let png = png();
        for broken in [&b"not a png"[..], &png[..png.len() - 1], &png[..20]] {
            assert_eq!(stamp(broken, &key("linux-x86_64")), None);
            assert_eq!(read(broken), None);
            assert!(matches!(strip(broken), Cow::Borrowed(_)));
        }
        // An origin whose text is no platform key (`Linux-x86_64`): signature, IHDR, then the
        // chunk's length and type, the keyword and its separator. It names no platform, so it
        // stays part of the image (a golden with it is no golden without it), until a stamp
        // replaces it.
        let mut bad = stamp(&png, &key("linux-x86_64")).unwrap();
        bad[33 + 8 + KEYWORD.len() + 1] = b'L';
        assert_eq!(read(&bad), None);
        assert!(matches!(strip(&bad), Cow::Borrowed(_)));
        assert_eq!(
            stamp(&bad, &key("macos-aarch64")).unwrap(),
            stamp(&png, &key("macos-aarch64")).unwrap()
        );
    }
}
