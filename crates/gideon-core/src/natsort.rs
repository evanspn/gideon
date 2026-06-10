//! Natural ("human") ordering for page file names.
//!
//! Plain lexicographic ordering sorts `page10.jpg` before `page2.jpg`, which
//! breaks reading order for the vast majority of CBZ files in the wild.
//! Natural ordering compares runs of digits by numeric value instead.

use std::cmp::Ordering;

/// Compare two strings using natural ordering, case-insensitively.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();

    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ac), Some(bc)) => {
                if ac.is_ascii_digit() && bc.is_ascii_digit() {
                    let an = take_number(&mut ai);
                    let bn = take_number(&mut bi);
                    match an.cmp(&bn) {
                        Ordering::Equal => continue,
                        other => return other,
                    }
                }

                let af = ac.to_ascii_lowercase();
                let bf = bc.to_ascii_lowercase();
                match af.cmp(&bf) {
                    Ordering::Equal => {
                        ai.next();
                        bi.next();
                    }
                    other => return other,
                }
            }
        }
    }
}

/// Consume a run of ASCII digits, returning its numeric value.
///
/// Leading zeros are ignored for comparison purposes; the value is compared
/// by (length-trimmed) magnitude so arbitrarily long digit runs still work.
fn take_number(iter: &mut std::iter::Peekable<std::str::Chars>) -> NumberRun {
    let mut digits = String::new();
    while let Some(c) = iter.peek().copied() {
        if c.is_ascii_digit() {
            digits.push(c);
            iter.next();
        } else {
            break;
        }
    }
    NumberRun(digits)
}

/// A run of digits compared by numeric magnitude without parsing into a
/// fixed-width integer (avoids overflow on pathological names).
#[derive(PartialEq, Eq)]
struct NumberRun(String);

impl Ord for NumberRun {
    fn cmp(&self, other: &Self) -> Ordering {
        let a = self.0.trim_start_matches('0');
        let b = other.0.trim_start_matches('0');
        a.len().cmp(&b.len()).then_with(|| a.cmp(b))
    }
}

impl PartialOrd for NumberRun {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(mut v: Vec<&str>) -> Vec<&str> {
        v.sort_by(|a, b| natural_cmp(a, b));
        v
    }

    #[test]
    fn numeric_runs_sort_by_value() {
        assert_eq!(
            sorted(vec!["page10.jpg", "page2.jpg", "page1.jpg"]),
            vec!["page1.jpg", "page2.jpg", "page10.jpg"]
        );
    }

    #[test]
    fn case_insensitive() {
        assert_eq!(
            sorted(vec!["B.png", "a.png", "C.png"]),
            vec!["a.png", "B.png", "C.png"]
        );
    }

    #[test]
    fn leading_zeros_are_equivalent() {
        assert_eq!(
            sorted(vec!["007.png", "8.png", "06.png"]),
            vec!["06.png", "007.png", "8.png"]
        );
    }

    #[test]
    fn mixed_segments() {
        assert_eq!(
            sorted(vec![
                "v2c10p3.jpg",
                "v2c2p1.jpg",
                "v10c1p1.jpg",
                "v2c10p1.jpg"
            ]),
            vec!["v2c2p1.jpg", "v2c10p1.jpg", "v2c10p3.jpg", "v10c1p1.jpg"]
        );
    }

    #[test]
    fn huge_numbers_do_not_overflow() {
        assert_eq!(
            sorted(vec!["99999999999999999999999999.png", "2.png"]),
            vec!["2.png", "99999999999999999999999999.png"]
        );
    }

    #[test]
    fn directory_prefixes() {
        assert_eq!(
            sorted(vec![
                "ch2/01.png",
                "ch10/01.png",
                "ch1/02.png",
                "ch1/01.png"
            ]),
            vec!["ch1/01.png", "ch1/02.png", "ch2/01.png", "ch10/01.png"]
        );
    }
}
