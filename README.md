# Vibrance

A fast, native music player for Linux, built with Rust and [iced](https://iced.rs).

## Run

```
sudo dnf install alsa-lib-devel opus-devel mold
cargo run --release -p vibrance
```

Import your music with Ctrl+O. `vibrance --help` lists the testing options.

The design lives in [SPEC.md](SPEC.md).

## License

MIT. Bundled fonts: Roboto Flex (OFL 1.1) and Material Symbols (Apache 2.0).
