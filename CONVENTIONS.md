# River Coding Conventions

- Keep files relatively short, ideally less than 200 lines.

- Organize files top-down, highest level functions / structs first.
- DO NOT CREATE mod.rs FILES, instead use the flat style for modules (ie. use foo.rs instead of foo/mod.rs)
- If you're having weird rsx issues read https://dioxuslabs.com/learn/0.6/guide/rsx/ and
  https://dioxuslabs.com/learn/0.6/essentials/rsx/#
- When working with Dioxus signals, avoid nested borrows of the same signal (e.g., don't call
  `write()` on a signal while already holding a `read()` reference). Instead, extract needed values
  into local variables before performing write operations.
