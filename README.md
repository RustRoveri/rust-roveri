# rust-roveri

## Quickstart

Add this to your `Cargo.toml`:

```toml
[dependencies]
rust-roveri = { git = "ssh://git@github.com/RustRoveri/rust-roveri.git" }
```

## Features

- Low memory usage (we avoid heap allocation on critical path when possible)
- Extensive testing (each use-case is tested at least once)
- Safety
  - Unchecked array indexing (`[...]`), `unwrap()` and `expect()` are never used
  - Every match expression is exhaustive and every if statement has an else branch
- Logging (we used the logging macro from the `log` crate)
- Debuggability (useful messages in the case of protocol illegal states)

## Support

If you are having trouble with our drone feel free to contact us on Telegram:
- @gggianlu
- @gioomaz
- @3michele
