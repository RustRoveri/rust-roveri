# rust-roveri

## Quickstart

Add this to your `Cargo.toml`:

```toml
[dependencies]
rust-roveri = { git = "ssh://git@github.com/RustRoveri/rust-roveri.git" }
```

## Features

- Safety
  - Unchecked array indexing (`[...]`), `unwrap()` and `expect()` are never used
  - Every match expression is exhaustive and every if statement has an else branch
- Blazingly fast (we avoid heap allocation on critical path when possible)
- Low memory usage
  - We use `BTreeSet` instead of `HashSet`
  - We avoid using `clone()` when possible
- Extensive testing (21 tests, around 1300 lines, each use-case is tested at least once)
- Logging (we used the logging macro from the `log` crate)
- Debuggability (useful messages in the case of protocol illegal states)

## Support

If you are having trouble with our drone feel free to contact us on Telegram:
- @gggianlu
- @gioomaz
- @3michele
