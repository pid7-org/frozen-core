[![Crates.io](https://img.shields.io/crates/v/frozen-core?style=flat-square&logo=rust)](https://crates.io/crates/frozen-core)
[![Docs.rs](https://img.shields.io/docsrs/frozen-core?style=flat-square&logo=rust)](https://docs.rs/frozen-core)
[![Tests](https://img.shields.io/github/actions/workflow/status/pid7-org/frozen-core/tests.yaml?style=flat-square&logo=github&label=tests)](https://github.com/pid7-org/frozen-core/actions/workflows/tests.yaml)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue?style=flat-square)](LICENSE-MIT)

# FrozenCore

Custom implementations and core utilities for [pid7-org](https://github.com/pid7-org/) crates

> [!NOTE]
> `frozen-core` contains next to naught AI-generated code
>
> Therefore, any catastrophic bugs or fatal crashes encountered are results of pure and unadulterated
> skill issues

## Feature Flags

- [`error`](https://docs.rs/frozen-core/latest/frozen_core/error/index.html)
- [`hints`](https://docs.rs/frozen-core/latest/frozen_core/hints/index.html)
- [`isa`](https://docs.rs/frozen-core/latest/frozen_core/isa/index.html)

## Usage

Add following to your `Cargo.toml`,

```toml
[dependencies]
frozen-core = { version = "0.0.33", default-features = true }
```

> [!IMPORTANT]
> Minimum Supported Rust Version (MSRV) is `1.86.0`

> [!TIP]
> All the features are enabled by default; to disable, set `default-features` to `false`

## TL;DR

> [!IMPORTANT]
> `frozen-core` is primarily created for [pid7-org](https://github.com/pid7-org/) projects
>
> External use is discouraged, but not prohibited, given __you assume all the risks__

This project is licensed under the Apache-2.0 and MIT License, see the [LICENSE-APACHE](LICENSE-APACHE) and
[LICENSE-MIT](LICENSE-MIT) file for more details

Contributions are welcome; please feel free to submit a PR or open an issue if you have any feedback or suggestions
