//! Search operators on top of the plain query (dtSearch-style, kept small):
//!
//! - `+kelime`, `kira VE depozito` / `AND`: the word must be in the document
//! - `-kelime`, `-"iki kelime"`, `DEĞİL kelime` / `NOT`: the document must not
//!   contain it
//! - `VEYA` / `OR`: either word (plain words already rank as "any of")
//! - `kira YAKIN/5 bedel` (also `w/5`, `near/5`): the two within 5 words, in
//!   either order, on the same page
//! - `kir*`, `dilek?e`: `*` any letters, `?` one letter
//! - `şimşek~`, `muhasebe~2`: also words one (or two) letters different
//!   (typos, OCR slips); ranks like a plain word
//! - `kira&`: the word's office synonyms (icar) count as much as the word
//!
//! Operator words are recognised only in capitals (`VE`, `VEYA`, `DEĞİL`,
//! `AND`, `OR`, `NOT`), so a query like "kira ve depozito" keeps its plain
//! meaning. `parse` returns the text the ranking still searches for (words
//! and quoted phrases, operators and excluded parts removed) and the
//! document-level constraints the engine applies to every result.

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryConstraints {
    /// Each entry: one word or a phrase (raw text, folded by the index).
    pub required: Vec<String>,
    pub excluded: Vec<String>,
    /// Two parts within `n` words of each other, in either order.
    pub near: Vec<(String, String, u32)>,
    /// Patterns with `*` / `?` that must match a word.
    pub wildcards: Vec<String>,
}

impl QueryConstraints {
    pub fn is_empty(&self) -> bool {
        self.required.is_empty() && self.excluded.is_empty() && self.near.is_empty() && self.wildcards.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedQuery {
    /// What the ranking searches for. Identical to the input when the query
    /// uses no operator.
    pub text: String,
    pub constraints: QueryConstraints,
    /// Extra ranking terms (fuzzy words, full-weight synonyms).
    pub extra: Vec<ExtraTerm>,
}

/// Ranking-only additions a query asks for; they widen what matches,
/// unlike `QueryConstraints`, which narrow it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExtraTerm {
    /// A word and the indexed words at most this many edits (1 or 2) away.
    Fuzzy(String, u8),
    /// A word's office synonyms at the same weight as the word.
    Synonyms(String),
}

/// `word~`, `word~2` → (word, distance); `word&` → word.
fn fuzzy_suffix(word: &str) -> Option<(&str, u8)> {
    let (base, tail) = word.rsplit_once('~')?;
    let distance = match tail {
        "" => 1,
        "1" => 1,
        "2" => 2,
        _ => return None,
    };
    (base.chars().filter(|c| c.is_alphanumeric()).count() >= 3).then_some((base, distance))
}

fn synonym_suffix(word: &str) -> Option<&str> {
    let base = word.strip_suffix('&')?;
    (base.chars().filter(|c| c.is_alphanumeric()).count() >= 2).then_some(base)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Token {
    Word(String),
    Phrase(String),
}

impl Token {
    fn raw(&self) -> &str {
        match self {
            Token::Word(w) | Token::Phrase(w) => w,
        }
    }

    fn as_text(&self) -> String {
        match self {
            Token::Word(w) => w.clone(),
            Token::Phrase(p) => format!("\"{p}\""),
        }
    }
}

const MAX_NEAR: u32 = 100;

fn near_distance(word: &str) -> Option<u32> {
    let lower = word.to_lowercase();
    let (name, number) = lower.split_once('/')?;
    if !matches!(name, "w" | "near" | "yakın" | "yakin") {
        return None;
    }
    number.parse::<u32>().ok().filter(|&n| n > 0).map(|n| n.min(MAX_NEAR))
}

fn is_and(word: &str) -> bool {
    matches!(word, "VE" | "AND" | "&&")
}

fn is_or(word: &str) -> bool {
    matches!(word, "VEYA" | "OR" | "||")
}

fn is_not(word: &str) -> bool {
    matches!(word, "DEĞİL" | "DEGIL" | "NOT")
}

/// Splits into words and quoted phrases; a sign glued to a word or phrase
/// (`-kira`, `+"iki kelime"`) is returned with it.
fn lex(query: &str) -> Vec<(Option<char>, Token)> {
    let normalized: String = query.chars().map(|c| if matches!(c, '“' | '”' | '„' | '«' | '»') { '"' } else { c }).collect();
    let mut out = Vec::new();
    let mut chars = normalized.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
            continue;
        }
        let mut sign = None;
        if c == '-' || c == '+' {
            chars.next();
            match chars.peek() {
                // A sign applies to a word or a quote, never to a number
                // ("-5 derece") or a lone dash.
                Some(&next) if next.is_alphabetic() || next == '"' => sign = Some(c),
                Some(&next) if !next.is_whitespace() => {
                    let mut word = String::from(c);
                    while let Some(&ch) = chars.peek() {
                        if ch.is_whitespace() || ch == '"' {
                            break;
                        }
                        word.push(ch);
                        chars.next();
                    }
                    out.push((None, Token::Word(word)));
                    continue;
                }
                _ => continue,
            }
        }
        if chars.peek() == Some(&'"') {
            chars.next();
            let mut phrase = String::new();
            let mut closed = false;
            for ch in chars.by_ref() {
                if ch == '"' {
                    closed = true;
                    break;
                }
                phrase.push(ch);
            }
            let phrase = phrase.trim().to_string();
            if !phrase.is_empty() {
                // An unclosed quote reads as plain words, as before.
                if closed {
                    out.push((sign, Token::Phrase(phrase)));
                } else {
                    out.extend(phrase.split_whitespace().map(|w| (None, Token::Word(w.to_string()))));
                }
            }
            continue;
        }
        let mut word = String::new();
        while let Some(&ch) = chars.peek() {
            if ch.is_whitespace() || ch == '"' {
                break;
            }
            word.push(ch);
            chars.next();
        }
        if !word.is_empty() {
            out.push((sign, Token::Word(word)));
        }
    }
    out
}

fn has_wildcard(word: &str) -> bool {
    word.contains(['*', '?'])
}

/// The letters before the first wildcard (ranking still uses them).
fn wildcard_stem(word: &str) -> &str {
    word.split(['*', '?']).next().unwrap_or("")
}

pub fn parse(query: &str) -> ParsedQuery {
    let tokens = lex(query);
    let mut constraints = QueryConstraints::default();
    let mut extra: Vec<ExtraTerm> = Vec::new();
    let mut text: Vec<String> = Vec::new();
    let mut operator = false;
    let mut i = 0;
    // Whether the previous kept term must be required by a following VE.
    let mut last_kept: Option<Token> = None;
    let mut pending_and = false;
    let mut pending_not = false;
    while i < tokens.len() {
        let (sign, token) = &tokens[i];
        let raw = token.raw();
        if sign.is_none() {
            if let Token::Word(word) = token {
                if is_and(word) {
                    operator = true;
                    if let Some(previous) = last_kept.take() {
                        push_unique(&mut constraints.required, required_text(&previous));
                    }
                    pending_and = true;
                    i += 1;
                    continue;
                }
                if is_or(word) {
                    operator = true;
                    pending_and = false;
                    i += 1;
                    continue;
                }
                if is_not(word) {
                    operator = true;
                    pending_not = true;
                    i += 1;
                    continue;
                }
                if let Some(distance) = near_distance(word) {
                    operator = true;
                    if let (Some(previous), Some((_, next))) = (last_kept.clone(), tokens.get(i + 1)) {
                        constraints.near.push((previous.raw().to_string(), next.raw().to_string(), distance));
                    }
                    i += 1;
                    continue;
                }
            }
        }
        let excluded = *sign == Some('-') || pending_not;
        let required = *sign == Some('+') || pending_and;
        pending_not = false;
        pending_and = false;
        if excluded {
            operator = true;
            push_unique(&mut constraints.excluded, raw.to_string());
            last_kept = None;
            i += 1;
            continue;
        }
        if let Token::Word(word) = token {
            if let Some((base, distance)) = fuzzy_suffix(word) {
                operator = true;
                if required {
                    push_unique(&mut constraints.required, base.to_string());
                }
                extra.push(ExtraTerm::Fuzzy(base.to_string(), distance));
                text.push(base.to_string());
                last_kept = Some(Token::Word(base.to_string()));
                i += 1;
                continue;
            }
            if let Some(base) = synonym_suffix(word) {
                operator = true;
                if required {
                    push_unique(&mut constraints.required, base.to_string());
                }
                extra.push(ExtraTerm::Synonyms(base.to_string()));
                text.push(base.to_string());
                last_kept = Some(Token::Word(base.to_string()));
                i += 1;
                continue;
            }
            if has_wildcard(word) {
                operator = true;
                if word.chars().filter(|c| c.is_alphanumeric()).count() >= 2 {
                    push_unique(&mut constraints.wildcards, word.clone());
                }
                let stem = wildcard_stem(word);
                if stem.chars().count() >= 2 {
                    text.push(stem.to_string());
                }
                last_kept = None;
                i += 1;
                continue;
            }
        }
        if required {
            operator = true;
            push_unique(&mut constraints.required, required_text(token));
        }
        text.push(token.as_text());
        last_kept = Some(token.clone());
        i += 1;
    }
    if !operator {
        return ParsedQuery { text: query.to_string(), constraints: QueryConstraints::default(), extra };
    }
    ParsedQuery { text: text.join(" "), constraints, extra }
}

fn required_text(token: &Token) -> String {
    token.raw().to_string()
}

fn push_unique(list: &mut Vec<String>, value: String) {
    if !value.trim().is_empty() && !list.contains(&value) {
        list.push(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_query_is_left_alone() {
        for query in ["kira sözleşmesi", "kira ve depozito", "\"kira bedeli\" 2024", "8.158,00 TL", "a-b"] {
            let parsed = parse(query);
            assert_eq!(parsed.text, query);
            assert!(parsed.constraints.is_empty(), "{query}");
        }
    }

    #[test]
    fn signs_and_capital_operators() {
        let parsed = parse("kira -depozito +sözleşme");
        assert_eq!(parsed.text, "kira sözleşme");
        assert_eq!(parsed.constraints.excluded, ["depozito"]);
        assert_eq!(parsed.constraints.required, ["sözleşme"]);

        let parsed = parse("kira VE depozito DEĞİL \"tahliye taahhüdü\"");
        assert_eq!(parsed.text, "kira depozito");
        assert_eq!(parsed.constraints.required, ["kira", "depozito"]);
        assert_eq!(parsed.constraints.excluded, ["tahliye taahhüdü"]);

        let parsed = parse("fatura VEYA makbuz NOT iptal");
        assert_eq!(parsed.text, "fatura makbuz");
        assert!(parsed.constraints.required.is_empty());
        assert_eq!(parsed.constraints.excluded, ["iptal"]);
    }

    #[test]
    fn proximity_and_wildcards() {
        let parsed = parse("kira YAKIN/5 bedel");
        assert_eq!(parsed.text, "kira bedel");
        assert_eq!(parsed.constraints.near, [("kira".to_string(), "bedel".to_string(), 5)]);
        assert_eq!(parse("apple w/3 pear").constraints.near, [("apple".to_string(), "pear".to_string(), 3)]);

        let parsed = parse("sözleş* dilek?e");
        assert_eq!(parsed.text, "sözleş dilek");
        assert_eq!(parsed.constraints.wildcards, ["sözleş*", "dilek?e"]);
    }

    #[test]
    fn fuzzy_and_synonym_suffixes() {
        let parsed = parse("şimşek~ muhasebe~2 kira&");
        assert_eq!(parsed.text, "şimşek muhasebe kira");
        assert!(parsed.constraints.is_empty());
        assert_eq!(
            parsed.extra,
            [
                ExtraTerm::Fuzzy("şimşek".into(), 1),
                ExtraTerm::Fuzzy("muhasebe".into(), 2),
                ExtraTerm::Synonyms("kira".into()),
            ]
        );
        // Too short, or not a distance: left as typed.
        assert_eq!(parse("ab~").extra, []);
        assert_eq!(parse("ab~5").text, "ab~5");
    }

    #[test]
    fn stray_signs_and_unclosed_quotes_do_not_break_the_query() {
        for query in ["kira - depozito", "-\"kira bedeli", "sıcaklık -5 derece", "kira +"] {
            let parsed = parse(query);
            assert!(parsed.constraints.is_empty(), "{query}: {parsed:?}");
            assert_eq!(parsed.text, query);
        }
    }
}
