use lazy_static::lazy_static;
use regex::Regex;

lazy_static! {
    static ref SPLITTERS: Vec<&'static str> = vec![
        ", ", "; ", " × ", " x ", " / ", " ft.", " ft. ", " feat. ", " & "
    ];
    static ref WHITELIST: Vec<&'static str> = vec![];
    static ref SPLITTERS_REGEX: Regex = {
        let splitters_pattern = SPLITTERS
            .iter()
            .map(|s| regex::escape(s))
            .collect::<Vec<String>>()
            .join("|");
        Regex::new(&splitters_pattern).unwrap()
    };
}

pub fn split_artists(input: &str) -> Vec<String> {
    let parts_with_splitters: Vec<&str> = SPLITTERS_REGEX.split(input).collect();
    let mut parts: Vec<String> = Vec::new();
    let mut start = 0; // Start index in characters

    for i in 0..parts_with_splitters.len() {
        let part = parts_with_splitters[i].trim();
        if !part.is_empty() {
            parts.push(part.to_string());
        }

        // Add the splitter back if it's not the last part
        if i < parts_with_splitters.len() - 1 {
            // Find the splitter in the remaining part of the input
            if let Some(mat) = SPLITTERS_REGEX.find(&input[start..]) {
                let splitter = mat.as_str();
                parts.push(splitter.to_string());
                // Update the start index to the end of the matched splitter
                start += mat.end();
            }
        }
    }

    let mut i = 0;
    while i < parts.len() {
        if WHITELIST.contains(&parts[i].as_str()) {
            i += 1;
            continue;
        }
        for j in (i + 1)..parts.len() {
            if SPLITTERS.contains(&parts[j].as_str()) {
                continue;
            }
            let combined = parts[i..=j].join("");
            if WHITELIST.contains(&combined.as_str()) {
                parts[i] = combined;
                parts.drain((i + 1)..=j);
                break;
            }
        }
        i += 1;
    }

    parts
        .into_iter()
        .filter(|s| !SPLITTERS.contains(&s.as_str()))
        .flat_map(|name| split_bare_slashes(&name))
        .collect()
}

/// Splits a name at slashes written without spaces when a side of the slash has Chinese,
/// Japanese or Korean characters, as in "澤野弘之/KOHTA YAMAMOTO". A slash between Latin
/// names, as in "AC/DC", is part of the name.
fn split_bare_slashes(name: &str) -> Vec<String> {
    // The text between slashes, each with the slash that ends it, half or full width.
    let mut segments: Vec<(&str, Option<char>)> = Vec::new();
    let mut start = 0;
    for (at, c) in name.char_indices() {
        if c == '/' || c == '／' {
            segments.push((&name[start..at], Some(c)));
            start = at + c.len_utf8();
        }
    }
    segments.push((&name[start..], None));

    let mut names = Vec::new();
    let mut current = String::new();
    for (index, (segment, slash)) in segments.iter().enumerate() {
        current.push_str(segment);
        let Some(slash) = slash else { continue };
        let next = segments[index + 1].0;
        if has_cjk(segment) || has_cjk(next) {
            names.push(std::mem::take(&mut current));
        } else {
            current.push(*slash);
        }
    }
    names.push(current);
    names
        .into_iter()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect()
}

/// Whether `text` has Chinese, Japanese or Korean characters.
fn has_cjk(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(
            c as u32,
            0x1100..=0x11FF // Hangul Jamo
                | 0x3040..=0x30FF // Hiragana and Katakana
                | 0x3130..=0x318F // Hangul Compatibility Jamo
                | 0x3400..=0x4DBF // CJK Unified Ideographs Extension A
                | 0x4E00..=0x9FFF // CJK Unified Ideographs
                | 0xAC00..=0xD7AF // Hangul Syllables
                | 0xF900..=0xFAFF // CJK Compatibility Ideographs
                | 0xFF66..=0xFF9F // Halfwidth Katakana
                | 0x20000..=0x3134F // CJK Unified Ideographs Extensions B to G
        )
    })
}

#[cfg(test)]
mod tests {
    use super::split_artists;

    #[test]
    fn credits_split_at_the_usual_separators() {
        assert_eq!(
            split_artists("Hiroyuki Sawano & SennaRin"),
            ["Hiroyuki Sawano", "SennaRin"]
        );
        assert_eq!(
            split_artists("One, Two feat. Three"),
            ["One", "Two", "Three"]
        );
        assert_eq!(split_artists("Left / Right"), ["Left", "Right"]);
        assert_eq!(split_artists("Solo"), ["Solo"]);
    }

    #[test]
    fn a_bare_slash_splits_next_to_cjk_names_only() {
        assert_eq!(
            split_artists("澤野弘之/KOHTA YAMAMOTO"),
            ["澤野弘之", "KOHTA YAMAMOTO"]
        );
        assert_eq!(
            split_artists("SennaRin／澤野弘之"),
            ["SennaRin", "澤野弘之"]
        );
        assert_eq!(split_artists("아이유/BTS"), ["아이유", "BTS"]);
        assert_eq!(split_artists("AC/DC"), ["AC/DC"]);
        assert_eq!(split_artists("AC/DC & 澤野弘之"), ["AC/DC", "澤野弘之"]);
        assert_eq!(split_artists("A/B/澤野弘之"), ["A/B", "澤野弘之"]);
        assert_eq!(split_artists("AC／DC"), ["AC／DC"]);
    }
}
