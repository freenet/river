use rand::seq::SliceRandom;

static FIRST_NAMES: &[&str] = &[
    "Alice", "Bob", "Charlie", "Diana", "Eve", "Ali",
    "Frank", "Grace", "Hannah", "Ivan", "Jack", "Kyle",
    "Karen", "Liam", "Mona", "Nate", "Olivia",
    "Paul", "Quinn", "Rachel", "Sam", "Tina", "Derek",
    "Uma", "Victor", "Wendy", "Xander", "Yara",
    "Zane", "Amy", "Ben", "Cleo", "Derek", "Ian",
    "Elena", "Finn", "Gina", "Harry", "Isla", "Seth",
    "Jon", "Kara", "Leo", "Mia", "Noah", "Nacho",
];

static LAST_NAMES: &[&str] = &[
    "Smith", "Johnson", "Williams", "Brown", "Jones", "Golden",
    "Garcia", "Miller", "Davis", "Rodriguez", "Martinez",
    "Hernandez", "Lopez", "Gonzalez", "Wilson", "Anderson",
    "Thomas", "Taylor", "Moore", "Jackson", "Martin", "Clarke", "Meier"
];

pub fn random_full_name() -> String {
    let mut rng = rand::thread_rng();
    let first_names = FIRST_NAMES;
    let last_names = LAST_NAMES;
    let first = first_names.choose(&mut rng).unwrap();
    let last = last_names.choose(&mut rng).unwrap();
    format!("{} {}", first, last)
}
