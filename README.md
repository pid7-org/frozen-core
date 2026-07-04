[![Latest Version](https://img.shields.io/crates/v/frozen_core.svg)](https://crates.io/crates/frozen_core)
[![License](https://img.shields.io/github/license/frozen-lab/frozen_core?logo=open-source-initiative&logoColor=white)](https://github.com/frozen-lab/frozen_core/blob/master/LICENSE)
[![Tests](https://github.com/frozen-lab/frozen_core/actions/workflows/tests.yaml/badge.svg)](https://github.com/frozen-lab/frozen_core/actions/workflows/tests.yaml)

# FrozenCore

Custom implementations and core utilities for [frozen-lab](https://github.com/frozen-lab/) crates.

## Usage

Add following to your `Cargo.toml`,

```toml
[dependencies]
frozen-core = { version = "0.0.32", default-features = true }
```

> [!NOTE]
> Current `frozen-core` requires Rust 1.86 or later.

> [!TIP]
> All the features are enabled by default. To disable, set `default-features` to `false`.

## Feature Flags

- [`ack`](https://docs.rs/frozen-core/latest/frozen_core/ack/index.html)
- [`error`](https://docs.rs/frozen-core/latest/frozen_core/error/index.html)
- [`hints`](https://docs.rs/frozen-core/latest/frozen_core/hints/index.html)
- [`ffile`](https://docs.rs/frozen-core/latest/frozen_core/ffile/index.html)
- [`fmmap`](https://docs.rs/frozen-core/latest/frozen_core/fmmap/index.html)
- [`crc32`](https://docs.rs/frozen-core/latest/frozen_core/crc32/index.html)
- [`mpscq`](https://docs.rs/frozen-core/latest/frozen_core/mpscq/index.html)
- [`bufpool`](https://docs.rs/frozen-core/latest/frozen_core/bufpool/index.html)
- [`wpipe`](https://docs.rs/frozen-core/latest/frozen_core/wpipe/index.html)
- [`reservoir`](https://docs.rs/frozen-core/latest/frozen_core/reservoir/index.html)

## Notes

> [!IMPORTANT]
> `frozen-core` is primarily created for [frozen-lab](https://github.com/frozen-lab/) projects. External use is
> discouraged, but not prohibited, given __you assume all the risks__.

This project is licensed under the Apache-2.0 and MIT License. See the [LICENSE-APACHE](LICENSE-APACHE) and
[LICENSE-MIT](LICENSE-MIT) file for more details.

Contributions are welcome! Please feel free to submit a PR or open an issue if you have any feedback or suggestions.

> [!NOTE]
> `frozen-core` contains next to naught AI-generated code. Therefore, any catastrophic bugs or fatal crashes encountered
> are results of pure & unadulterated skill issues
