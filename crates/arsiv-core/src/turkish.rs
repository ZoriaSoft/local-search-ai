//! Turkish search normalization, mirroring the measured laboratory rules.
//!
//! `İ`→`i` and `I`→`ı` are applied *before* Unicode lowercasing; generic
//! lowercasing alone maps `I` to `i`, which corrupts Turkish words.

use unicode_normalization::UnicodeNormalization;

/// NFC + Turkish-aware lowercase.
pub fn turkish_lower(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.nfc() {
        match ch {
            'I' => out.push('ı'),
            'İ' => out.push('i'),
            other => out.extend(other.to_lowercase()),
        }
    }
    out.nfc().collect()
}

/// Search-only ASCII folding of Turkish letters (never for OCR scoring).
pub fn ascii_fold(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            'ç' => 'c',
            'ğ' => 'g',
            'ı' => 'i',
            'ö' => 'o',
            'ş' => 's',
            'ü' => 'u',
            'â' => 'a',
            'î' => 'i',
            'û' => 'u',
            'Ç' => 'C',
            'Ğ' => 'G',
            'İ' => 'I',
            'Ö' => 'O',
            'Ş' => 'S',
            'Ü' => 'U',
            'Â' => 'A',
            'Î' => 'I',
            'Û' => 'U',
            other => other,
        })
        .collect()
}

/// Content-word tokens; internal apostrophes stay inside a token (Ankara'da).
pub fn tokenize(text: &str) -> Vec<(usize, usize)> {
    token_spans(text).collect()
}

/// Lazy form of `tokenize`, so consumers such as snippets can stop without
/// scanning or allocating spans for the rest of a large page.
pub fn token_spans(text: &str) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut chars = text.char_indices().peekable();
    std::iter::from_fn(move || loop {
        let (start, ch) = chars.next()?;
        if !is_word(ch) {
            continue;
        }
        let mut end = start + ch.len_utf8();
        while let Some(&(index, next)) = chars.peek() {
            if is_word(next) {
                end = index + next.len_utf8();
                chars.next();
            } else if next == '\'' || next == '’' {
                let mut lookahead = chars.clone();
                lookahead.next();
                match lookahead.peek() {
                    Some(&(_, after)) if is_word(after) => {
                        chars.next();
                    }
                    _ => break,
                }
            } else {
                break;
            }
        }
        return Some((start, end));
    })
}

fn is_word(ch: char) -> bool {
    ch.is_alphanumeric()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotted_and_dotless_i_keep_turkish_meaning() {
        assert_eq!(turkish_lower("ISPARTA İSTANBUL ışık"), "ısparta istanbul ışık");
        assert_eq!(turkish_lower("SÖZLEŞMESİ"), "sözleşmesi");
    }

    #[test]
    fn ascii_folding_matches_unaccented_queries() {
        assert_eq!(ascii_fold(&turkish_lower("Öğrenci Şirketi")), "ogrenci sirketi");
    }

    #[test]
    fn apostrophe_suffix_stays_in_token_but_trailing_quote_does_not() {
        let text = "Ankara'da T.C.'nin 'alıntı'";
        let tokens: Vec<&str> = tokenize(text).into_iter().map(|(s, e)| &text[s..e]).collect();
        assert_eq!(tokens, ["Ankara'da", "T", "C", "nin", "alıntı"]);
    }
}
