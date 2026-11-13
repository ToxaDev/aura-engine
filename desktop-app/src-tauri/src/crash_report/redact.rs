//! What keeps a person's music, names and addresses out of a report.
//!
//! Three layers, each for what the one before it cannot see:
//!
//! 1. **Known values** become stable tokens (`track#3`, `file#7`, `<user>`):
//!    every path, file name, stem and tag the session met, the home folder,
//!    the user and computer names, the audio devices, the stations. Without
//!    regard to case; a slash and a backslash count as the same character.
//! 2. **Shapes**: absolute paths, addresses, e-mails, anything with an audio
//!    extension, any word in a script the log itself is not written in.
//! 3. **The last look**: a known value still in the line takes the whole line
//!    with it, down to its time and tag.
//!
//! A value of fewer than 3 characters or only of digits is not "known": it
//! would take the sample rates and every short word in the log with it. One
//! shorter than 8 only counts as a whole word: an artist called "Air" is no
//! reason to lose "repair".

use aho_corasick::{AhoCorasick, MatchKind};
use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

/// Longer than this is not a log line anyone needs in full.
const LINE_MAX_CHARS: usize = 600;
/// Below this a value counts only as a whole word.
const WHOLE_WORD_BELOW: usize = 8;
const MIN_CHARS: usize = 3;

/// Lower case, slashes one way; with the original byte each folded byte came
/// from, and one last entry for the end.
fn fold(s: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(s.len());
    let mut from = Vec::with_capacity(s.len() + 1);
    for (i, c) in s.char_indices() {
        let c = if c == '/' { '\\' } else { c };
        for l in c.to_lowercase() {
            out.push(l);
            from.resize(out.len(), i);
        }
    }
    from.push(s.len());
    (out, from)
}

fn folded(s: &str) -> String {
    fold(s).0
}

fn is_word_char(c: Option<char>) -> bool {
    c.is_some_and(char::is_alphanumeric)
}

/// Replaces known values, and answers whether any is in a text.
pub struct Redactor {
    values: Vec<String>,
    tokens: Vec<String>,
    /// Counted in characters, for the whole-word rule.
    short: Vec<bool>,
    seen: HashSet<String>,
    matcher: Option<AhoCorasick>,
}

impl Redactor {
    pub fn new() -> Redactor {
        Redactor { values: Vec::new(), tokens: Vec::new(), short: Vec::new(), seen: HashSet::new(), matcher: None }
    }

    /// `value` becomes `token`. The first token given to a value stands.
    pub fn add(&mut self, value: &str, token: &str) {
        let value = value.trim();
        let chars = value.chars().count();
        if chars < MIN_CHARS || value.chars().all(|c| c.is_ascii_digit()) {
            return;
        }
        let f = folded(value);
        if !self.seen.insert(f.clone()) {
            return;
        }
        self.values.push(f);
        self.tokens.push(token.to_string());
        self.short.push(chars < WHOLE_WORD_BELOW);
        self.matcher = None;
    }

    /// A path, its file name and its stem, under one token (and its folder).
    pub fn add_path(&mut self, path: &str, token: &str) {
        self.add(path, token);
        let p = std::path::Path::new(path);
        // The last part by either separator: a Windows path read elsewhere,
        // and the other way round.
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        self.add(name, token);
        if let Some((stem, _)) = name.rsplit_once('.') {
            self.add(stem, token);
        }
        if let Some(parent) = p.parent().map(|d| d.to_string_lossy().into_owned()) {
            self.add(&parent, "<dir>");
        }
    }

    /// Ready for use (the matcher is built once, after the last `add`).
    pub fn finish(&mut self) {
        self.matcher = AhoCorasick::builder()
            .match_kind(MatchKind::Standard)
            .build(&self.values)
            .ok()
            .filter(|_| !self.values.is_empty());
    }

    /// The known values in `f` (a folded text), as (start, end, value index):
    /// leftmost first, the longest at a place, none over another.
    fn matches_in(&self, f: &str) -> Vec<(usize, usize, usize)> {
        let Some(m) = &self.matcher else { return Vec::new() };
        let mut all: Vec<(usize, usize, usize)> = m
            .find_overlapping_iter(f)
            .map(|x| (x.start(), x.end(), x.pattern().as_usize()))
            .filter(|&(s, e, v)| {
                !self.short[v] || (!is_word_char(f[..s].chars().next_back()) && !is_word_char(f[e..].chars().next()))
            })
            .collect();
        all.sort_by_key(|&(s, e, _)| (s, std::cmp::Reverse(e)));
        let mut kept = Vec::with_capacity(all.len());
        let mut reached = 0;
        for m in all {
            if m.0 >= reached {
                reached = m.1;
                kept.push(m);
            }
        }
        kept
    }

    /// Layer 1.
    pub fn replace_known(&self, s: &str) -> String {
        let (f, from) = fold(s);
        let mut out = String::with_capacity(s.len());
        let mut at = 0;
        for (start, end, v) in self.matches_in(&f) {
            let (a, b) = (from[start], from[end]);
            if a < at {
                continue;
            }
            out.push_str(&s[at..a]);
            out.push_str(&self.tokens[v]);
            at = b;
        }
        out.push_str(&s[at..]);
        out
    }

    /// Layer 3's question.
    pub fn has_known(&self, s: &str) -> bool {
        !self.matches_in(&folded(s)).is_empty()
    }

    /// All three layers over a log line.
    pub fn line(&self, s: &str) -> String {
        let (head, body) = split_head(s);
        let body = cut(body);
        let out = shapes(&self.replace_known(body), true);
        if self.has_known(&out) {
            return format!("[line dropped] {}", head.trim());
        }
        format!("{head}{out}")
    }

    /// All three layers over a value of the report.
    pub fn text(&self, s: &str) -> String {
        let out = shapes(&self.replace_known(s), true);
        if self.has_known(&out) { "[dropped]".to_string() } else { out }
    }

    /// The same over what the user wrote: their own language stays (they see
    /// it before it goes), a name of a file or an address does not.
    pub fn note(&self, s: &str) -> String {
        let out = shapes(&self.replace_known(s), false);
        if self.has_known(&out) { "[dropped]".to_string() } else { out }
    }
}

fn cut(s: &str) -> &str {
    match s.char_indices().nth(LINE_MAX_CHARS) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// `[12:00:00.000] [TAG] ` and the rest: the head says when and where, the
/// program wrote it, and it survives a dropped line.
fn split_head(s: &str) -> (&str, &str) {
    static HEAD: OnceLock<Regex> = OnceLock::new();
    let re = HEAD.get_or_init(|| Regex::new(r"^\[\d\d:\d\d:\d\d\.\d{3}\]\s*(?:\[[A-Za-z0-9 _-]{1,24}\]\s*)?").unwrap());
    match re.find(s) {
        Some(m) => s.split_at(m.end()),
        None => ("", s),
    }
}

const AUDIO_EXT: &str = "flac|wav|wave|aif|aiff|aifc|mp3|mp2|m4a|m4b|aac|alac|ogg|oga|opus|wma|ape|wv|dsf|dff|dsd|tta|mka|mp4|m3u8?|pls|cue";

struct Shapes {
    url: Regex,
    host_path: Regex,
    email: Regex,
    unc: Regex,
    drive: Regex,
    unix: Regex,
    audio: Regex,
    script: Regex,
    device: Regex,
    guid: Regex,
    long_hex: Regex,
    mac: Regex,
}

fn compiled() -> &'static Shapes {
    static S: OnceLock<Shapes> = OnceLock::new();
    S.get_or_init(|| Shapes {
        url: Regex::new(r#"(?i)\b[a-z][a-z0-9+.\-]{1,15}://[^\s"'<>]+"#).unwrap(),
        host_path: Regex::new(r#"(?i)\b[a-z0-9\-]+(?:\.[a-z0-9\-]+)*\.[a-z]{2,}(?::\d+)?/[^\s"'<>]*"#).unwrap(),
        email: Regex::new(r"[\w.+\-]+@[\w\-]+\.[\w.\-]+").unwrap(),
        // A path runs up to a character it cannot hold or the end of the line:
        // a name has spaces and commas in it. A token inside (the user's) goes
        // with the path around it.
        unc: Regex::new(r#"\\\\[^\s\\/"'|*?:]+[\\/][^"'|*?:\r\n]*"#).unwrap(),
        drive: Regex::new(r#"(?i)\b[a-z]:[\\/][^"'|*?:\r\n]*"#).unwrap(),
        unix: Regex::new(r#"(^|[\s"'=(\[,:])(?:~/|~?/[^/\s"'|*?:]+/)[^"'|*?:\r\n]*"#).unwrap(),
        // A file name without its folders: back to the nearest mark that
        // cannot be in the middle of one as logged.
        audio: Regex::new(&format!(r#"(?i)[^"'=:|\r\n]*\.(?:{AUDIO_EXT})\b"#)).unwrap(),
        script: Regex::new(r"\S*[\p{L}&&[^\x00-\x7F]]\S*").unwrap(),
        // A device as the output's lines quote it, listed or not.
        device: Regex::new(r#"(?i)\bdevice=(?:"[^"]*"|'[^']*')"#).unwrap(),
        // Ids that outlive a session: an audio endpoint's, a card's, an adapter's.
        guid: Regex::new(r"(?i)\{?[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\}?").unwrap(),
        long_hex: Regex::new(r"(?i)\b[0-9a-f]{16,}\b").unwrap(),
        mac: Regex::new(r"(?i)\b(?:[0-9a-f]{2}[:\-]){5}[0-9a-f]{2}\b").unwrap(),
    })
}

/// Layer 2. `all_words`: also words in a script the log is not written in.
fn shapes(s: &str, all_words: bool) -> String {
    let c = compiled();
    let s = s.replace(r"\\?\", "");
    let s = c.device.replace_all(&s, "device=<device>");
    let s = c.guid.replace_all(&s, "<id>");
    let s = c.long_hex.replace_all(&s, "<id>");
    let s = c.mac.replace_all(&s, "<id>");
    let s = c.url.replace_all(&s, "<url>");
    let s = c.host_path.replace_all(&s, "<url>");
    let s = c.email.replace_all(&s, "<email>");
    let s = c.unc.replace_all(&s, "<path>");
    let s = c.drive.replace_all(&s, "<path>");
    let s = c.unix.replace_all(&s, "$1<path>");
    let s = c.audio.replace_all(&s, "<file>");
    if all_words {
        c.script.replace_all(&s, "<text>").into_owned()
    } else {
        s.into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(values: &[(&str, &str)]) -> Redactor {
        let mut r = Redactor::new();
        for (v, t) in values {
            r.add(v, t);
        }
        r.finish();
        r
    }

    const NONE: &[(&str, &str)] = &[];

    #[test]
    fn an_id_that_outlives_a_session_does_not_go() {
        let r = with(NONE);
        assert_eq!(
            r.line("[12:00:01.123] [UI] device: {0.0.0.00000000}.{14ad898c-4e89-4348-897e-e4edca92000b}"),
            "[12:00:01.123] [UI] device: {0.0.0.00000000}.<id>"
        );
        assert_eq!(
            r.line(r#"[12:00:01.123] [PLAYER] stream open: device="Gone Since" rate=88200"#),
            "[12:00:01.123] [PLAYER] stream open: device=<device> rate=88200"
        );
        assert_eq!(r.line("[12:00:01.123] [X] a 0a1b2c3d4e5f60718293a4b5 b aa:bb:cc:dd:ee:ff c"), "[12:00:01.123] [X] a <id> b <id> c");
        assert_eq!(r.line("[12:00:01.123] [GPU] vendor=0x10de device=0x2684 88200 1M"), "[12:00:01.123] [GPU] vendor=0x10de device=0x2684 88200 1M");
    }

    #[test]
    fn a_bare_title_from_the_players_lines_goes_by_value() {
        let r = with(&[("Hotel California", "track#3"), ("Eagles", "tag#1")]);
        assert_eq!(
            r.line("[12:00:01.123] [PLAYER] lamp of Hotel California: 500 MB free past the margin, 900 MB needed"),
            "[12:00:01.123] [PLAYER] lamp of track#3: 500 MB free past the margin, 900 MB needed"
        );
        assert_eq!(
            r.line("[12:00:01.123] [PLAYER] HOTEL california: its start gives way to what the listener asked"),
            "[12:00:01.123] [PLAYER] track#3: its start gives way to what the listener asked"
        );
    }

    #[test]
    fn cyrillic_and_japanese_values_match_in_any_case() {
        let r = with(&[("Кино — Группа крови", "track#1"), ("夜に駆ける", "track#2")]);
        assert_eq!(r.line("[12:00:01.123] [PLAYER] КИНО — группа КРОВИ: waited 5 ms"), "[12:00:01.123] [PLAYER] track#1: waited 5 ms");
        assert_eq!(r.line("[12:00:01.123] [PLAYER] 夜に駆ける: waited 5 ms"), "[12:00:01.123] [PLAYER] track#2: waited 5 ms");
    }

    #[test]
    fn a_path_a_name_and_a_stem_share_one_token() {
        let mut r = Redactor::new();
        r.add_path(r"D:\Музыка\Daft Punk\01 - Get Lucky.flac", "file#7");
        r.finish();
        assert!(r.line(r"[CONV] Re-decoding: D:\Музыка\Daft Punk\01 - Get Lucky.flac").ends_with("file#7"));
        assert!(r.line("[CONV] Decoding: 01 - Get Lucky.flac").ends_with("file#7"));
        assert!(r.line("[PLAYER] 01 - get lucky: waited").contains("file#7: waited"));
        assert!(r.line("same D:/Музыка/Daft Punk/01 - Get Lucky.flac").contains("file#7"), "the other slash");
    }

    #[test]
    fn a_short_value_counts_only_as_a_whole_word() {
        let r = with(&[("Air", "tag#1"), ("U2", "tag#2"), ("1979", "track#1")]);
        assert_eq!(r.line("repair the chair, Air played"), "repair the chair, tag#1 played");
        assert_eq!(r.line("U2 at 1979 Hz"), "U2 at 1979 Hz", "too short, digits: not values at all");
    }

    #[test]
    fn the_longest_value_at_a_place_wins() {
        let r = with(&[("Daft Punk", "tag#1"), ("Daft Punk - Get Lucky", "track#1")]);
        assert_eq!(r.line("Daft Punk - Get Lucky and Daft Punk"), "track#1 and tag#1");
    }

    #[test]
    fn windows_paths_go_whole_with_their_spaces() {
        let r = with(NONE);
        assert_eq!(r.line(r"[CONV] loading C:\Users\Иван Петров\Music\a, b\c d.flac"), "[CONV] loading <path>");
        assert_eq!(r.line(r"loading c:/users/x/y.bin: done"), "loading <path>: done");
        assert_eq!(r.line(r"open \\?\C:\Music\x y.wav failed"), "open <path>");
    }

    #[test]
    fn unc_paths_go_whole() {
        let r = with(NONE);
        assert_eq!(r.line(r"[PLAYER] Cannot open \\nas\Музыка\Daft Punk\x.flac: denied"), "[PLAYER] Cannot open <path>: denied");
    }

    #[test]
    fn unix_paths_go_whole() {
        let r = with(NONE);
        assert_eq!(r.line("open /home/ivan/My Music/a b.flac"), "open <path>");
        assert_eq!(r.line("open '/Users/ivan/Music/a.flac' failed"), "open '<path>' failed");
        assert_eq!(r.line("see ~/Music/x"), "see <path>");
        assert_eq!(r.line("rate 44100/2 x FS/8 and a / b"), "rate 44100/2 x FS/8 and a / b");
    }

    #[test]
    fn addresses_and_mail_go() {
        let r = with(&[("http://ice1.somafm.com/groovesalad-256-mp3", "<url#2>")]);
        assert_eq!(r.line("[RADIO] tune http://ice1.somafm.com/groovesalad-256-mp3 now"), "[RADIO] tune <url#2> now");
        assert_eq!(r.line("[RADIO] tune https://stream.example.org:8000/live?key=abc now"), "[RADIO] tune <url> now");
        assert_eq!(r.line("[RADIO] host stream.example.org:8000/live now"), "[RADIO] host <url> now");
        assert_eq!(r.line("write to ivan.petrov+x@mail.example.com now"), "write to <email> now");
    }

    #[test]
    fn a_bare_file_name_goes_back_to_its_mark() {
        let r = with(NONE);
        assert_eq!(r.line("[12:00:00.000] [CONV] Decoding: Eagles - Hotel California (Live, 1994) [A].flac"), "[12:00:00.000] [CONV] Decoding:<file>");
        assert_eq!(r.line("x=My Song.OPUS y=1"), "x=<file> y=1");
    }

    #[test]
    fn words_in_another_script_go_even_unknown() {
        let r = with(NONE);
        assert_eq!(r.line("[12:00:00.000] [PLAYER] Любовь: waited 5 ms"), "[12:00:00.000] [PLAYER] <text> waited 5 ms");
        assert_eq!(r.line("夜に駆ける and 1 more"), "<text> and 1 more");
        assert_eq!(r.line("−0.5 dB × 8 — ok"), "−0.5 dB × 8 — ok", "signs are not words");
    }

    #[test]
    fn the_log_s_own_lines_stay_readable() {
        let r = with(&[("Hotel California", "track#1")]);
        for l in [
            "[12:00:00.000] [WATCHDOG] stall control: no change for 31 s | control=play p=0 still=31.0s; prep=idle | ram free 5 MB vram free ?",
            "[12:00:00.000] [MEM] ram free 12345 MB vram free 2048 MB",
            "[12:00:00.000] [GPU] wola block 44100 Hz, 1048576 taps, FS8",
        ] {
            assert_eq!(r.line(l), l);
        }
    }

    #[test]
    fn a_known_value_a_layer_let_through_drops_the_line_keeping_its_head() {
        // A value that only comes to be by what the layers did: "a<url>b" has
        // none, but a token that spells a value must not pass.
        let r = with(&[("<file>", "x"), ("kept secret", "track#1")]);
        let line = r.line("[12:00:00.000] [CONV] name a.flac");
        assert_eq!(line, "[line dropped] [12:00:00.000] [CONV]");
    }

    #[test]
    fn the_note_keeps_its_language_but_not_its_paths() {
        let r = with(&[("Hotel California", "track#1")]);
        assert_eq!(r.note("Вылетает на Hotel California, файл D:\\Музыка\\x.flac"), "Вылетает на track#1, файл <path>");
    }

    #[test]
    fn a_very_long_line_is_cut() {
        let r = with(NONE);
        assert!(r.line(&"x".repeat(5000)).len() <= LINE_MAX_CHARS);
    }

    #[test]
    fn folding_keeps_bytes_right_when_lower_case_is_longer() {
        let r = with(&[("İstanbul", "track#1")]);
        assert_eq!(r.replace_known("a İSTANBUL b"), "a track#1 b");
    }
}
