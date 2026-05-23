use rand::seq::SliceRandom;

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

pub fn random() -> String {
    let mut rng = rand::thread_rng();
    let a = ADJECTIVES.choose(&mut rng).unwrap();
    let m = MONTHS.choose(&mut rng).unwrap();
    let n = ANIMALS.choose(&mut rng).unwrap();
    format!("{a}-{m}-{n}")
}

/// Generate a random `[a-z0-9]` string of the given length.
pub fn random_lowercase(len: usize) -> String {
    let chars: Vec<char> = "abcdefghijklmnopqrstuvwxyz0123456789".chars().collect();
    let mut rng = rand::thread_rng();
    (0..len).map(|_| *chars.choose(&mut rng).unwrap()).collect()
}

/// Generate a random mixed-case alphanumeric string of the given length.
/// Used for tokens where case-sensitivity isn't a barrier and the wider alphabet
/// gives more bits per character.
pub fn random_alphanumeric(len: usize) -> String {
    let chars: Vec<char> = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
        .chars()
        .collect();
    let mut rng = rand::thread_rng();
    (0..len).map(|_| *chars.choose(&mut rng).unwrap()).collect()
}

pub fn comment_id() -> String {
    format!("c_{}", random_lowercase(6))
}

pub fn delete_token() -> String {
    random_lowercase(24)
}
