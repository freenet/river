use rand::seq::SliceRandom;

static FIRST_NAMES: &[&str] = &[
    "Alice", "Bob", "Charlie", "Diana", "Eve", "Ali", "Frank", "Grace", "Hannah", "Ivan", "Jack",
    "Kyle", "Karen", "Liam", "Mona", "Nate", "Olivia", "Paul", "Quinn", "Rachel", "Sam", "Tina",
    "Derek", "Uma", "Victor", "Wendy", "Xander", "Yara", "Zane", "Amy", "Ben", "Cleo", "Derek",
    "Ian", "Elena", "Finn", "Gina", "Harry", "Isla", "Seth", "Jon", "Kara", "Leo", "Mia", "Noah",
    "Nacho",
];

static LAST_NAMES: &[&str] = &[
    "Smith",
    "Johnson",
    "Williams",
    "Brown",
    "Jones",
    "Golden",
    "Garcia",
    "Miller",
    "Davis",
    "Rodriguez",
    "Martinez",
    "Hernandez",
    "Lopez",
    "Gonzalez",
    "Wilson",
    "Anderson",
    "Thomas",
    "Taylor",
    "Moore",
    "Jackson",
    "Martin",
    "Clarke",
    "Meier",
];

/// Every name [`random_full_name`] can produce, in pool order.
///
/// Test-only. A property that must hold for EVERY generated handle should be
/// enumerated, not sampled: the product is only 46 x 23 = 1,058 names, and
/// sampling turns a pool regression into an intermittent CI failure — which
/// this repo treats as a broken test, not a flaky one.
#[cfg(test)]
pub(crate) fn all_full_names() -> impl Iterator<Item = String> {
    FIRST_NAMES
        .iter()
        .flat_map(|first| LAST_NAMES.iter().map(move |last| format!("{first} {last}")))
}

pub fn random_full_name() -> String {
    let mut rng = rand::thread_rng();
    let first = FIRST_NAMES.choose(&mut rng).unwrap();
    let last = LAST_NAMES.choose(&mut rng).unwrap();
    format!("{} {}", first, last)
}
