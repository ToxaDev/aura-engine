//! Tag text as it was written.
//!
//! An ID3v2 frame marked ISO-8859-1, and every ID3v1 tag, is decoded by
//! symphonia one byte to one character. Taggers of the 2000s put the Windows
//! code page of their system into those frames, so a Russian title comes out
//! as Latin letters with accents: "Ãåîìåòðèÿ" for "Геометрия". The bytes are
//! all still there — Latin-1 gives each of them its own character — except
//! 0x80–0x9F, which the ID3v2 reader turns into U+FFFD. So the text can be
//! read again the way it was meant, and that is all this does: it never
//! guesses at a byte that is gone.
//!
//! A UTF-16 frame is read by its byte-order mark, and without one as
//! big-endian, as ID3v2 says. Some taggers write little-endian with no mark
//! (or with the wrong one), and every character comes out with its two bytes
//! swapped: "BM" as "䈀䴀". Such text is swapped back (`byte_swapped`).
//!
//! Only ID3 is read again. FLAC and Ogg (Vorbis comments), MP4 and APE are
//! Unicode by their specifications, and what they say is taken as it is.

/// Windows-1251 for 0x80–0xBF; 0xC0–0xFF are U+0410–U+044F in order. 0x98
/// is not assigned and keeps its C1 code point.
const CP1251_80: [char; 64] = [
    '\u{0402}', '\u{0403}', '\u{201A}', '\u{0453}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{20AC}', '\u{2030}', '\u{0409}', '\u{2039}', '\u{040A}', '\u{040C}', '\u{040B}', '\u{040F}',
    '\u{0452}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{0098}', '\u{2122}', '\u{0459}', '\u{203A}', '\u{045A}', '\u{045C}', '\u{045B}', '\u{045F}',
    '\u{00A0}', '\u{040E}', '\u{045E}', '\u{0408}', '\u{00A4}', '\u{0490}', '\u{00A6}', '\u{00A7}',
    '\u{0401}', '\u{00A9}', '\u{0404}', '\u{00AB}', '\u{00AC}', '\u{00AD}', '\u{00AE}', '\u{0407}',
    '\u{00B0}', '\u{00B1}', '\u{0406}', '\u{0456}', '\u{0491}', '\u{00B5}', '\u{00B6}', '\u{00B7}',
    '\u{0451}', '\u{2116}', '\u{0454}', '\u{00BB}', '\u{0458}', '\u{0405}', '\u{0455}', '\u{0457}',
];

/// Windows-1252 for 0x80–0x9F, the codes Latin-1 leaves to control
/// characters. The five it does not assign keep their C1 code point.
const CP1252_80: [char; 32] = [
    '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}', '\u{017D}', '\u{008F}',
    '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
];

fn cp1251(b: u8) -> char {
    match b {
        0x00..=0x7F => b as char,
        0x80..=0xBF => CP1251_80[(b - 0x80) as usize],
        _ => char::from_u32(0x0410 + (b - 0xC0) as u32).unwrap_or('\u{FFFD}'),
    }
}

fn cp1252_c1(c: char) -> char {
    match c as u32 {
        0x80..=0x9F => CP1252_80[(c as u32 - 0x80) as usize],
        _ => c,
    }
}

fn is_cyrillic(c: char) -> bool {
    ('\u{0400}'..='\u{04FF}').contains(&c)
}

/// One byte per character again — None where the reader left U+FFFD — or
/// None for text that is no Latin-1 reading: real Unicode, or plain ASCII.
fn latin1_bytes(raw: &str) -> Option<Vec<Option<u8>>> {
    if raw.is_ascii() || raw.chars().any(|c| c as u32 > 0xFF && c != '\u{FFFD}') {
        return None;
    }
    Some(raw.chars().map(|c| if c == '\u{FFFD}' { None } else { Some(c as u8) }).collect())
}

/// Whether the bytes are Windows-1251 beyond doubt: a word of three Cyrillic
/// letters or more, one of them А–я at least. Western text in Latin-1 puts
/// its accented letters between plain ones ("Beyoncé", "Mötley Crüe") or
/// alone ("è così", "à toi", "Þú"); no word of a Latin-1 language is three
/// of them in a row.
fn sure_1251(bytes: &[Option<u8>]) -> bool {
    let (mut len, mut core, mut cyr_only) = (0, false, true);
    for b in bytes.iter().copied().chain(std::iter::once(None)) {
        match b.map(cp1251) {
            Some(c) if is_cyrillic(c) => {
                len += 1;
                core |= matches!(b, Some(0xC0..=0xFF));
            }
            Some(c) if c.is_ascii_alphabetic() => {
                len += 1;
                cyr_only = false;
            }
            _ => {
                if cyr_only && core && len >= 3 {
                    return true;
                }
                (len, core, cyr_only) = (0, false, true);
            }
        }
    }
    false
}

/// UTF-8 written into a Latin-1 frame, also when an ID3v1 field's 30 bytes
/// cut it inside a character. Not ASCII, so at least one multi-byte
/// sequence: Latin-1 text almost never makes one by chance (and a Latin-1
/// "Café" cut before its "é" is plain ASCII, and is not taken for UTF-8).
fn utf8(bytes: &[Option<u8>]) -> Option<String> {
    let whole: Vec<u8> = bytes.iter().copied().collect::<Option<_>>()?;
    let s = match std::str::from_utf8(&whole) {
        Ok(s) => s,
        Err(e) if e.error_len().is_none() => std::str::from_utf8(&whole[..e.valid_up_to()]).ok()?,
        Err(_) => return None,
    };
    (!s.is_ascii()).then(|| s.to_string())
}

/// Whether a reading turns up in the file's name (Windows keeps names in
/// Unicode): a confirmation however short it is ("Òû" in "05 - Ты.mp3").
fn in_name(cyr: &str, name: &str) -> bool {
    let t = cyr.trim();
    !t.contains('\u{FFFD}')
        && t.chars().any(is_cyrillic)
        && name.to_lowercase().contains(&t.to_lowercase())
}

/// Whether a file's ID3 text is Windows-1251: one of its tags is, beyond
/// doubt (and is not UTF-8, whose Cyrillic bytes spell words in 1251 too).
/// Then its other tags read so too, however short ("Мы", "Би-2").
pub fn file_reads_1251<'a>(id3_texts: impl IntoIterator<Item = &'a str>) -> bool {
    id3_texts.into_iter().any(|t| latin1_bytes(t).is_some_and(|b| utf8(&b).is_none() && sure_1251(&b)))
}

/// Whether a character, its bytes swapped back, is one a Latin, Greek or
/// Cyrillic title is written in: letters, digits, punctuation and symbols.
fn readable(c: char) -> bool {
    matches!(c as u32, 0x20..=0x7E | 0xA0..=0x52F | 0x2010..=0x206F | 0x20A0..=0x20CF | 0x2100..=0x214F)
}

/// UTF-16 read with its bytes swapped, read back: "䈀䴀" → "BM".
///
/// The text is taken for swapped when every character of it, swapped back,
/// is `readable` and as it stands not all are (a swapped space alone is
/// U+2000). One character alone could be a real CJK title, so it is
/// swapped only when the file's name has it ("嘀" → "V" in "… - V").
/// A U+FFFD stays: symphonia lost that character (a swapped Ø–ß falls on a
/// surrogate).
fn byte_swapped(raw: &str, name: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let (mut n, mut was_readable) = (0, true);
    for c in raw.chars() {
        if c == '\u{FFFD}' {
            out.push(c);
            continue;
        }
        let u = u16::try_from(c as u32).ok()?;
        let back = char::from_u32(u.swap_bytes() as u32).filter(|&b| readable(b))?;
        out.push(back);
        was_readable &= readable(c);
        n += 1;
    }
    // A word: some letter or digit beyond Latin-1's symbols and the
    // accented capitals a Hangul syllable can swap into ("가" → "¬", "저" → "È").
    let word = out.chars().any(|c| c.is_ascii_alphanumeric() || ('\u{100}'..='\u{52F}').contains(&c));
    match n {
        _ if was_readable || !word => None,
        1 if !name.to_lowercase().contains(&out.trim().to_lowercase()) => None,
        _ => Some(out),
    }
}

/// An ID3 tag's text as its tagger wrote it, and whether it was read as
/// Windows-1251.
///
/// `file_1251`: the file's answer (`file_reads_1251`). `name`: the file's
/// name without its extension, which confirms a short reading on its own.
///
/// Real Unicode and plain ASCII come back as they are. Otherwise, in order:
/// UTF-8 written into a Latin-1 frame; Windows-1251 when the file reads so
/// or its name has the reading; anything else stays Latin-1, with what falls
/// on its control codes read as Windows-1252. A U+FFFD stays U+FFFD.
///
/// UTF-16 read with its bytes swapped is swapped back first; when it lost a
/// character on the way in, it comes back empty, and the caller takes the
/// file's name, as for a missing tag.
pub fn id3_text(raw: &str, file_1251: bool, name: &str) -> (String, bool) {
    if let Some(t) = byte_swapped(raw, name) {
        return (if t.contains('\u{FFFD}') { String::new() } else { t }, false);
    }
    let Some(bytes) = latin1_bytes(raw) else { return (raw.to_string(), false) };
    if let Some(s) = utf8(&bytes) {
        return (s, false);
    }
    let cyr: String = bytes.iter().map(|b| b.map_or('\u{FFFD}', cp1251)).collect();
    if file_1251 || in_name(&cyr, name) {
        return (cyr, true);
    }
    (raw.chars().map(cp1252_c1).collect(), false)
}

/// One text field of a file, gathered over the metadata revisions its reader
/// holds: ID3v2's before ID3v1's (a field there keeps 30 bytes, so a longer
/// title is cut), otherwise the first one given.
#[derive(Default)]
pub struct Field(Option<(String, bool, bool)>); // text, from ID3, from ID3v1

impl Field {
    /// `reader`: the revision's `info.short_name` ("id3v2", "id3v1", "flac"…).
    pub fn offer(&mut self, reader: &str, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let v1 = reader == "id3v1";
        if self.0.as_ref().map_or(true, |(_, _, was_v1)| *was_v1 && !v1) {
            self.0 = Some((text.to_string(), reader.starts_with("id3"), v1));
        }
    }

    /// The text as the reader gave it, when an ID3 tag gave it.
    pub fn id3(&self) -> Option<&str> {
        self.0.as_ref().filter(|f| f.1).map(|f| f.0.as_str())
    }

    /// The text to show, and whether it was read as Windows-1251. Any other
    /// reader's text is taken as it is.
    pub fn read(&self, file_1251: bool, name: &str) -> (String, bool) {
        match &self.0 {
            Some((t, true, _)) => id3_text(t, file_1251, name),
            Some((t, false, _)) => (t.clone(), false),
            None => (String::new(), false),
        }
    }
}

/// MP3 files for the tests that read tags through symphonia.
#[cfg(test)]
pub(crate) mod test_mp3 {
    /// An ID3v2.3 text frame: encoding byte 0 (ISO-8859-1), the text, and
    /// the terminating null the tagger wrote.
    pub fn id3_text_frame(id: &[u8; 4], text: &[u8]) -> Vec<u8> {
        let mut body = vec![0x00];
        body.extend_from_slice(text);
        body.push(0x00);
        frame(id, &body)
    }

    /// A UTF-16 text frame: the encoding byte (1 — with a byte-order mark,
    /// 2 — big-endian), the bytes as given (mark and all) and the two-byte
    /// null. A frame under 128 bytes has the same size field in ID3v2.3 and
    /// 2.4 (whose size is syncsafe).
    pub fn id3_utf16_frame(id: &[u8; 4], encoding: u8, text: &[u8]) -> Vec<u8> {
        let mut body = vec![encoding];
        body.extend_from_slice(text);
        body.extend_from_slice(&[0, 0]);
        frame(id, &body)
    }

    fn frame(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut f = id.to_vec();
        f.extend_from_slice(&(body.len() as u32).to_be_bytes());
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(body);
        f
    }

    /// An MP3 as a tagger leaves it: ID3v2.3 in front, ten silent MPEG-1
    /// Layer III frames (128 kbit/s, 44.1 kHz, mono, side info all zero),
    /// and an ID3v1 tag at the end when one is given (title, artist, album —
    /// each cut to the 30 bytes the format has).
    pub fn make_mp3(frames: &[Vec<u8>], v1: Option<(&[u8], &[u8], &[u8])>) -> Vec<u8> {
        let body = frames.concat();
        let n = body.len() as u32;
        let mut out = b"ID3\x03\x00\x00".to_vec();
        out.extend_from_slice(&[(n >> 21) as u8 & 0x7F, (n >> 14) as u8 & 0x7F, (n >> 7) as u8 & 0x7F, n as u8 & 0x7F]);
        out.extend_from_slice(&body);
        for _ in 0..10 {
            out.extend_from_slice(&[0xFF, 0xFB, 0x90, 0xC0]);
            out.extend_from_slice(&[0u8; 413]);
        }
        if let Some((title, artist, album)) = v1 {
            let mut tag = [0u8; 128];
            tag[..3].copy_from_slice(b"TAG");
            for (at, text) in [(3, title), (33, artist), (63, album)] {
                let n = text.len().min(30);
                tag[at..at + n].copy_from_slice(&text[..n]);
            }
            tag[127] = 0xFF;
            out.extend_from_slice(&tag);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What symphonia's ID3v2 reader makes of a Latin-1 frame: a character
    /// per byte, U+FFFD for the C0 and C1 control codes.
    fn id3v2_latin1(bytes: &[u8]) -> String {
        bytes
            .iter()
            .map(|&b| match b {
                0x00..=0x09 | 0x0B..=0x1F | 0x80..=0x9F => '\u{FFFD}',
                _ => b as char,
            })
            .collect()
    }

    /// And its ID3v1 reader: a character per byte, nothing lost.
    fn id3v1_latin1(bytes: &[u8]) -> String {
        bytes.iter().map(|&b| b as char).collect()
    }

    /// A tag read the way the player reads a file of this one tag.
    fn alone(raw: &str, name: &str) -> (String, bool) {
        id3_text(raw, file_reads_1251([raw]), name)
    }

    // The tag of «Пыльца - Геометрия.mp3» (Anton's library, 1.10): TIT2 and
    // TPE1 in Windows-1251, marked ISO-8859-1.
    const TITLE: &[u8] = &[0xC3, 0xE5, 0xEE, 0xEC, 0xE5, 0xF2, 0xF0, 0xE8, 0xFF];
    const ARTIST: &[u8] = b"[mp3ex.net]\xCF\xFB\xEB\xFC\xF6\xE0";

    #[test]
    fn a_windows_1251_tag_reads_as_it_was_written() {
        let name = "Пыльца - Геометрия";
        assert_eq!(id3v2_latin1(TITLE), "Ãåîìåòðèÿ", "what the player showed");
        assert_eq!(alone(&id3v2_latin1(TITLE), name), ("Геометрия".into(), true));
        assert_eq!(alone(&id3v2_latin1(ARTIST), name), ("[mp3ex.net]Пыльца".into(), true));
        assert_eq!(alone(&id3v1_latin1(TITLE), name), ("Геометрия".into(), true));
        // The words carry it without the name.
        assert_eq!(alone(&id3v2_latin1(TITLE), "").0, "Геометрия");
        assert_eq!(alone(&id3v2_latin1(ARTIST), "track 04").0, "[mp3ex.net]Пыльца");
        // Ukrainian letters and Ё are Cyrillic too.
        assert_eq!(alone(&id3v1_latin1(b"\xB2\xE2\xE0\xED \xB8\xEB\xEA\xE0"), "").0, "Іван ёлка");
    }

    #[test]
    fn a_short_tag_reads_so_with_the_file_or_the_name() {
        let ty = id3v1_latin1(b"\xD2\xFB"); // «Ты»
        // Two letters alone could be Latin-1 ("Þú").
        assert_eq!(alone(&ty, "track05"), ("Òû".into(), false));
        // The name has it.
        assert_eq!(alone(&ty, "05 - Ты"), ("Ты".into(), true));
        // Another tag of the file is Windows-1251 beyond doubt.
        let album = id3v2_latin1(b"\xCC\xFB"); // «Мы»
        let cyr = file_reads_1251([id3v2_latin1(TITLE).as_str(), album.as_str()]);
        assert!(cyr);
        assert_eq!(id3_text(&album, cyr, ""), ("Мы".into(), true));
        assert_eq!(id3_text(&id3v2_latin1(b"\xC1\xE8-2"), cyr, ""), ("Би-2".into(), true));
    }

    #[test]
    fn western_words_of_one_or_two_accented_letters_stay_latin() {
        // Three one-letter words summed to "Cyrillic" once (review 1.10).
        for s in ["Che cos'è, è così, è l'amore", "Þú komst í hlaðið", "À toi, à moi, à nous", "¡¡¡ Hola"] {
            assert!(!file_reads_1251([s]), "{s}");
            assert_eq!(alone(s, "01 track"), (s.to_string(), false), "{s}");
        }
    }

    #[test]
    fn a_lost_byte_stays_lost() {
        // «Пыльца — Геометрия»: the dash is 0x97, which the ID3v2 reader drops.
        let bytes = b"\xCF\xFB\xEB\xFC\xF6\xE0 \x97 \xC3\xE5\xEE\xEC\xE5\xF2\xF0\xE8\xFF";
        assert_eq!(alone(&id3v2_latin1(bytes), ""), ("Пыльца \u{FFFD} Геометрия".into(), true));
        // From ID3v1 the byte is still there.
        assert_eq!(alone(&id3v1_latin1(bytes), "").0, "Пыльца — Геометрия");
    }

    #[test]
    fn latin_1_text_is_left_as_it_is() {
        for s in [
            "Beyoncé", "Mötley Crüe", "Sigur Rós - Ágætis byrjun", "Größe", "Þú", "Björk", "¿Qué?", "São João",
            "Motörhead", "Café del Mar", "Ça plane pour moi", "Señorita", "Øresund", "Ænima",
        ] {
            assert!(!file_reads_1251([s]), "{s}");
            assert_eq!(alone(s, "01 track"), (s.to_string(), false), "{s}");
        }
    }

    #[test]
    fn unicode_and_ascii_are_left_as_they_are() {
        for s in ["Пыльца", "Геометрия (Live)", "東京", "AC/DC", "", "Ñandú 東"] {
            assert_eq!(id3_text(s, true, "Пыльца - Геометрия"), (s.to_string(), false));
        }
    }

    #[test]
    fn utf_8_in_a_latin_1_frame_is_read_as_utf_8() {
        assert_eq!(alone(&id3v2_latin1("Café".as_bytes()), "").0, "Café");
        assert_eq!(alone(&id3v1_latin1("Пыльца".as_bytes()), "").0, "Пыльца");
        // Its bytes spell 1251 words too («РџС‹»); they decide nothing for the file.
        assert!(!file_reads_1251([id3v1_latin1("Пыльца".as_bytes()).as_str()]));
        // An ID3v1 field keeps 30 bytes, and here they end inside «и».
        let cut = &"Пыльца - Геометрия".as_bytes()[..30];
        assert_eq!(alone(&id3v1_latin1(cut), ""), ("Пыльца - Геометр".into(), false));
    }

    #[test]
    fn windows_1252_punctuation_in_id3v1_is_read_as_such() {
        assert_eq!(alone(&id3v1_latin1(b"Don\x92t Stop \x96 Live"), "").0, "Don’t Stop – Live");
    }

    #[test]
    fn a_field_takes_id3v2_over_id3v1_and_repairs_only_id3() {
        for order in [["id3v1", "id3v2"], ["id3v2", "id3v1"]] {
            let mut f = Field::default();
            for reader in order {
                f.offer(reader, if reader == "id3v1" { "Short" } else { "The whole title" });
            }
            assert_eq!(f.read(false, "").0, "The whole title", "{order:?}");
        }
        let mut f = Field::default();
        f.offer("id3v2", "");
        f.offer("id3v1", "Kept");
        assert_eq!(f.read(false, "").0, "Kept", "an empty tag is no tag");
        // A Vorbis comment is Unicode: whatever it says stands.
        let mut f = Field::default();
        f.offer("vorbis", "Ãåîìåòðèÿ");
        assert_eq!(f.id3(), None);
        assert_eq!(f.read(true, "Геометрия"), ("Ãåîìåòðèÿ".into(), false));
    }

    /// What symphonia makes of little-endian UTF-16 it reads as big-endian.
    fn swapped(s: &str) -> String {
        char::decode_utf16(s.encode_utf16().map(u16::swap_bytes)).map(|r| r.unwrap_or('\u{FFFD}')).collect()
    }

    #[test]
    fn utf_16_read_with_its_bytes_swapped_is_read_back() {
        // As in Anton's 1.9.0 log (26.10).
        assert_eq!(swapped("BM"), "䈀䴀");
        for s in ["BM", "When It's War", "Sophie, Shygirl", "Marcin Przybyłowicz", "Привет, мир", "Ёлка", "Don’t Stop – 2077"] {
            assert_eq!(alone(&swapped(s), ""), (s.to_string(), false), "{s}");
        }
        // One character: only with the file's name behind it.
        assert_eq!(alone(&swapped("V"), "Marcin Przybyłowicz - V").0, "V");
        assert_eq!(alone(&swapped("V"), "01 track").0, "嘀");
    }

    #[test]
    fn swapped_text_that_lost_a_letter_comes_back_empty() {
        // A swapped "Ü" falls on a surrogate, which symphonia turns into U+FFFD.
        let raw = swapped("Müller Über");
        assert!(raw.contains('\u{FFFD}'), "{raw}");
        assert_eq!(alone(&raw, "Müller Über").0, "");
    }

    #[test]
    fn real_cjk_text_stays_as_it_is() {
        // "가" swaps to "¬", "가저" to "¬È", "一" to "N" (and the name has no N).
        for s in [
            "你好", "中文", "東京事変", "ひらがな", "カタカナ", "ありがとう", "一", "사랑해", "가", "가저", "Live 東京",
            "東京 2020", "BTS 방탄소년단 1", "ア 1",
        ] {
            assert_eq!(alone(s, "").0, s, "{s}");
        }
    }
}
