# arthron

**This package is a placeholder. It contains no executable code and installing it does nothing.**

[arthron](https://github.com/RandomCodeSpace/arthron) is a local-first code
intelligence engine written in Rust. It parses each file in isolation, then
resolves the references between them into a verified graph — and records the
references it could not resolve rather than dropping them.

It is in the design phase. The approved design is public:
**https://github.com/RandomCodeSpace/arthron**

## Why this package exists

The tool will ship as a native binary. npm is planned as one distribution
channel — the same route [ast-grep](https://www.npmjs.com/package/@ast-grep/cli)
takes, so that `npx arthron` works without a Rust toolchain. This package holds
that name until there is a binary to put in it.

There is nothing to install yet. When there is, this README will be replaced by
real usage documentation and the version will move off `0.0.0`.

## Meanwhile

The Rust crate is published and equally not-yet-usable:
[crates.io/crates/arthron](https://crates.io/crates/arthron). It exposes the
resolution contract the design is built around — nothing more.

## License

MIT
