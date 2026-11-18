// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Pure-Rust replacement for ICU bindings to avoid dynamic library dependencies.

use std::cmp::Ordering;
use std::fmt;
use std::mem::MaybeUninit;
use std::ops::Range;

use crate::arena::Arena;
use crate::buffer::TextBuffer;
use crate::collections::BString;

pub(crate) const ILLEGAL_ARGUMENT_ERROR: Error = Error(1); // U_ILLEGAL_ARGUMENT_ERROR
pub const ICU_MISSING_ERROR: Error = Error(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(pub u32);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ICU Error: {:#08x}", self.0)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy)]
pub struct Encoding {
    pub label: &'static str,
    pub canonical: &'static str,
}

pub struct Encodings {
    pub preferred: &'static [Encoding],
    pub all: &'static [Encoding],
}

static ENCODINGS_LIST: [Encoding; 2] = [
    Encoding { label: "UTF-8", canonical: "UTF-8" },
    Encoding { label: "UTF-8 BOM", canonical: "UTF-8 BOM" },
];

static ENCODINGS: Encodings = Encodings {
    preferred: &ENCODINGS_LIST,
    all: &ENCODINGS_LIST,
};

/// Returns a list of encodings supported.
pub fn get_available_encodings() -> &'static Encodings {
    &ENCODINGS
}

/// Converts between two encodings.
pub struct Converter<'pivot> {
    _marker: std::marker::PhantomData<&'pivot ()>,
}

impl<'pivot> Converter<'pivot> {
    pub fn new(
        _pivot_buffer: &'pivot mut [MaybeUninit<u16>],
        source_encoding: &str,
        target_encoding: &str,
    ) -> Result<Self> {
        let is_utf8 = |enc: &str| enc.eq_ignore_ascii_case("UTF-8") || enc.eq_ignore_ascii_case("UTF-8 BOM");
        if is_utf8(source_encoding) && is_utf8(target_encoding) {
            Ok(Self { _marker: std::marker::PhantomData })
        } else {
            Err(ILLEGAL_ARGUMENT_ERROR)
        }
    }

    pub fn convert(
        &mut self,
        input: &[u8],
        output: &mut [MaybeUninit<u8>],
    ) -> Result<(usize, usize)> {
        let len = input.len().min(output.len());
        for i in 0..len {
            output[i].write(input[i]);
        }
        Ok((len, len))
    }
}

pub struct Text {
    pub(crate) tb: *const TextBuffer,
}

impl Text {
    pub unsafe fn new(tb: &TextBuffer) -> Result<Self> {
        Ok(Self { tb: tb as *const TextBuffer })
    }

    pub(crate) unsafe fn extract_string(&self) -> String {
        let tb = unsafe { &*self.tb };
        let len = tb.text_length();
        let mut bytes = Vec::with_capacity(len);
        let chunk1 = tb.read_forward(0);
        bytes.extend_from_slice(chunk1);
        let chunk2 = tb.read_forward(chunk1.len());
        bytes.extend_from_slice(chunk2);
        String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
    }
}

pub struct Regex {
    re: regex::Regex,
    text_cache: String,
    offset: usize,
    last_caps: Option<Vec<Option<Range<usize>>>>,
    pattern_has_caret: bool,
    pattern_has_dollar: bool,
}

impl Regex {
    pub const CASE_INSENSITIVE: i32 = 2;
    pub const MULTILINE: i32 = 8;
    pub const LITERAL: i32 = 16;

    pub unsafe fn new(pattern: &str, flags: i32, text: &Text) -> Result<Self> {
        let pattern_has_caret = pattern.contains('^');
        let pattern_has_dollar = pattern.contains('$');

        let pattern_escaped = if flags & Self::LITERAL != 0 {
            regex::escape(pattern)
        } else {
            pattern.to_string()
        };

        let mut builder = regex::RegexBuilder::new(&pattern_escaped);
        builder.multi_line(flags & Self::MULTILINE != 0);
        builder.case_insensitive(flags & Self::CASE_INSENSITIVE != 0);
        builder.crlf(true);

        let re = builder.build().map_err(|_| ILLEGAL_ARGUMENT_ERROR)?;
        let text_cache = unsafe { text.extract_string() };

        Ok(Self {
            re,
            text_cache,
            offset: 0,
            last_caps: None,
            pattern_has_caret,
            pattern_has_dollar,
        })
    }

    pub fn next(&mut self) -> Result<Option<Range<usize>>> {
        if self.offset > self.text_cache.len() {
            self.last_caps = None;
            return Ok(None);
        }

        while self.offset < self.text_cache.len() && !self.text_cache.is_char_boundary(self.offset) {
            self.offset += 1;
        }

        if let Some(caps) = self.re.captures_at(&self.text_cache, self.offset) {
            let main_match = caps.get(0).unwrap();
            let match_range = main_match.range();

            // UTS #18 / ICU multiline rule: `^` does not match at the very end of input after a trailing newline.
            if match_range.is_empty()
                && match_range.start == self.text_cache.len()
                && (self.text_cache.ends_with('\n') || self.text_cache.ends_with('\r'))
                && self.pattern_has_caret
                && !self.pattern_has_dollar
            {
                self.last_caps = None;
                return Ok(None);
            }

            let mut stored_caps = Vec::with_capacity(caps.len());
            for i in 0..caps.len() {
                stored_caps.push(caps.get(i).map(|m| m.range()));
            }
            self.last_caps = Some(stored_caps);

            if match_range.end > self.offset {
                self.offset = match_range.end;
            } else {
                if let Some(ch) = self.text_cache[self.offset..].chars().next() {
                    self.offset += ch.len_utf8();
                } else {
                    self.offset = self.text_cache.len() + 1;
                }
            }

            Ok(Some(match_range))
        } else {
            self.last_caps = None;
            Ok(None)
        }
    }

    pub unsafe fn set_text(&mut self, text: &mut Text, offset: usize) {
        self.text_cache = unsafe { text.extract_string() };
        self.offset = offset.min(self.text_cache.len());
        self.last_caps = None;
    }

    pub fn reset(&mut self, offset: usize) {
        self.offset = offset.min(self.text_cache.len());
        self.last_caps = None;
    }

    pub fn group_count(&mut self) -> i32 {
        (self.re.captures_len() as i32).saturating_sub(1)
    }

    pub fn group(&mut self, group: i32) -> Option<Range<usize>> {
        if let Some(caps) = &self.last_caps {
            if let Some(Some(range)) = caps.get(group as usize) {
                return Some(range.clone());
            }
        }
        None
    }
}

pub fn compare_strings(a: &[u8], b: &[u8]) -> Ordering {
    compare_strings_ascii(a, b)
}

fn compare_strings_ascii(a: &[u8], b: &[u8]) -> Ordering {
    let mut iter = a.iter().zip(b.iter());

    while let Some((&a, &b)) = iter.next() {
        if a != b {
            let la = a.to_ascii_lowercase();
            let lb = b.to_ascii_lowercase();
            let mut order = la.cmp(&lb);

            if order == Ordering::Equal {
                order = a.cmp(&b);

                for (a, b) in iter {
                    let la = a.to_ascii_lowercase();
                    let lb = b.to_ascii_lowercase();

                    if la != lb {
                        order = la.cmp(&lb);
                        break;
                    }
                }
            }

            return order;
        }
    }

    a.len().cmp(&b.len())
}

pub fn fold_case<'a>(arena: &'a Arena, input: &str) -> BString<'a> {
    let mut result = BString::from_str(arena, input);
    for b in unsafe { result.as_bytes_mut() } {
        b.make_ascii_lowercase();
    }
    result
}

pub fn init() -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init() {
        assert!(super::init().is_ok());
    }

    #[test]
    fn test_compare_strings_ascii() {
        assert_eq!(compare_strings_ascii(b"", b""), Ordering::Equal);
        assert_eq!(compare_strings_ascii(b"hello", b"hello"), Ordering::Equal);
        assert_eq!(compare_strings_ascii(b"abc", b"abcd"), Ordering::Less);
        assert_eq!(compare_strings_ascii(b"abcd", b"abc"), Ordering::Greater);
        assert_eq!(compare_strings_ascii(b"AbC", b"aBc"), Ordering::Less);
        assert_eq!(compare_strings_ascii(b"a", b"B"), Ordering::Less);
        assert_eq!(compare_strings_ascii(b"B", b"a"), Ordering::Greater);
        assert_eq!(compare_strings_ascii(b"hallo", b"Hello"), Ordering::Less);
        assert_eq!(compare_strings_ascii(b"Hello", b"hallo"), Ordering::Greater);
    }
}
