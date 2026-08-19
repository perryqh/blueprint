use rand::Rng;
use rand::distributions::{Alphanumeric, DistString, Uniform};

const ADJECTIVES: &[&str] = &[
    "clear", "quiet", "bright", "rapid", "steady", "calm", "bold", "humble", "merry", "lively",
    "swift", "gentle", "wise", "happy", "fresh", "noble", "kind", "open", "warm", "loyal",
    "patient", "stoic", "snug", "dapper",
];

const MONTHS: &[&str] = &[
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];

const ANIMALS: &[&str] = &[
    "fox", "otter", "owl", "lynx", "heron", "swan", "raven", "wolf", "bear", "stag", "moose",
    "lark", "finch", "crane", "hawk", "kite", "robin", "marten", "badger", "puffin", "ibis",
    "mink", "shrew", "weasel",
];

/// Length of the entropy suffix welded onto the animal.
///
/// The word lists alone give 24 × 12 × 24 = 6,912 slugs. By the birthday bound
/// that's a ~50% chance of a collision at only ~98 blueprints, and the create
/// path is a bare INSERT — a collision surfaces as a raw SQLITE_CONSTRAINT
/// behind a 500. Four `[a-z0-9]` characters multiply the space by 36^4, to
/// ~11.6M, which pushes the 50% point past 4,000 blueprints.
///
/// It reads as a fourth segment (`clear-june-fox-7k2z`) rather than being welded
/// onto the animal, because a slug is a URL a human retypes and the boundary
/// makes it scannable.
const SUFFIX_LEN: usize = 4;

pub fn random() -> String {
    let mut rng = rand::thread_rng();
    // Indexing with `gen_range` rather than `choose().unwrap()`: the lists are
    // non-empty consts, so there was never a `None` to handle, and this makes
    // that structural instead of a runtime assertion.
    let a = ADJECTIVES[rng.gen_range(0..ADJECTIVES.len())];
    let m = MONTHS[rng.gen_range(0..MONTHS.len())];
    let n = ANIMALS[rng.gen_range(0..ANIMALS.len())];
    format!("{a}-{m}-{n}-{}", random_lowercase(SUFFIX_LEN))
}

/// The `[a-z0-9]` alphabet, as bytes so `Uniform` can index it directly.
const LOWER_ALNUM: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// Generate a random `[a-z0-9]` string of the given length.
/// `sample_iter` draws straight from the distribution — no per-call `Vec<char>`
/// to build and no `unwrap` to justify.
pub fn random_lowercase(len: usize) -> String {
    let dist = Uniform::new(0, LOWER_ALNUM.len());
    rand::thread_rng()
        .sample_iter(dist)
        .take(len)
        .map(|i| LOWER_ALNUM[i] as char)
        .collect()
}

/// Generate a random mixed-case alphanumeric string of the given length.
/// Used for tokens where case-sensitivity isn't a barrier and the wider alphabet
/// gives more bits per character.
pub fn random_alphanumeric(len: usize) -> String {
    Alphanumeric.sample_string(&mut rand::thread_rng(), len)
}

pub fn comment_id() -> String {
    format!("c_{}", random_lowercase(6))
}

pub fn delete_token() -> String {
    random_lowercase(24)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// adjective-month-animal-suffix, with the suffix as its own segment so the
    /// slug stays scannable when a human retypes it out of the URL bar.
    #[test]
    fn random_slug_is_three_words_plus_an_entropy_suffix() {
        for _ in 0..200 {
            let s = random();
            let parts: Vec<&str> = s.split('-').collect();
            assert_eq!(parts.len(), 4, "expected four segments: {s}");
            assert!(ADJECTIVES.contains(&parts[0]), "bad adjective: {s}");
            assert!(MONTHS.contains(&parts[1]), "bad month: {s}");
            assert!(ANIMALS.contains(&parts[2]), "bad animal: {s}");
            assert_eq!(parts[3].len(), SUFFIX_LEN, "wrong suffix length: {s}");
            assert!(
                parts[3]
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()),
                "suffix outside [a-z0-9]: {s}"
            );
        }
    }

    /// The whole point of the suffix: 6,912 slugs collided far too readily.
    #[test]
    fn random_slugs_do_not_collide_across_a_realistic_corpus() {
        let seen: std::collections::HashSet<String> = (0..5_000).map(|_| random()).collect();
        // A handful of collisions in 5k draws from an ~11.6M space is plausible;
        // the pre-suffix 6,912-slug space would have collapsed to ~6k uniques.
        assert!(
            seen.len() > 4_990,
            "too many collisions: {} uniques",
            seen.len()
        );
    }

    #[test]
    fn generators_respect_length_and_alphabet() {
        let lower = random_lowercase(24);
        assert_eq!(lower.len(), 24);
        assert!(
            lower
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        );

        let alnum = random_alphanumeric(32);
        assert_eq!(alnum.len(), 32);
        assert!(alnum.bytes().all(|b| b.is_ascii_alphanumeric()));

        assert!(random_lowercase(0).is_empty());
    }
}
