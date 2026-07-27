# @randomcodespace/arthron

**This package is a placeholder. It contains no executable code and installing it does nothing.**

[arthron](https://github.com/RandomCodeSpace/arthron) is a local-first code
intelligence engine written in Rust. It parses each file in isolation, then
resolves the references between them into a verified graph — and records the
references it could not resolve rather than dropping them.

The engine is early — scanning works for Go, built from source. See the
repository: **https://github.com/RandomCodeSpace/arthron**

## Why this package exists

The tool will ship as a native binary. npm is planned as one distribution
channel — the same route [ast-grep](https://www.npmjs.com/package/@ast-grep/cli)
takes, so `npx @randomcodespace/arthron` works without a Rust toolchain. This
package holds that name until there is a binary to put in it.

There is nothing to install yet. When there is, this README will be replaced by
real usage documentation and the version will move off `0.0.0`.

## Registry

Published to **GitHub Packages**, not npmjs.org. Consuming it needs a scope
mapping and a GitHub token with `read:packages`:

```
# .npmrc
@randomcodespace:registry=https://npm.pkg.github.com
//npm.pkg.github.com/:_authToken=${GITHUB_TOKEN}
```

## Meanwhile

The `0.0.0` crate on [crates.io](https://crates.io/crates/arthron) predates the
engine and is equally a placeholder. Build from source until both move off
`0.0.0`.

## License

MIT
