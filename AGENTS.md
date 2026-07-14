# AGENTS.md

See `README.md` for the full daemon documentation (config schema, backends, systemd unit).

## Cursor Cloud specific instructions

Rust stable (edition-2024 capable) is preinstalled in the VM; the startup update script runs `cargo fetch`. Standard commands:

- Build / test / lint: `cargo build`, `cargo test --all-features`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`.
- The default build uses an **in-memory stub backend** (no libzmq, no ports) and is always safe to build/run. Enabling `--all-features` / the `corpus-ipc` feature pulls the `corpus-ipc` git dependency and builds ZeroMQ from vendored C++ source via `zmq-sys`. That vendored build needs a working C++ compiler; the VM's `c++`/`cc` are g++/gcc (clang here cannot find libstdc++ headers). If a `fatal error: 'string' file not found` build error reappears, restore it with `sudo update-alternatives --auto c++ && sudo update-alternatives --auto cc`.
- Run the daemon: `cargo build --release --bin soma-daemon`, then `./target/release/soma-daemon --config <path.toml>`.
- It expects a TOML (Tom's Obvious, Minimal Language) configuration file. The platform-dependent default is typically `~/.config/soma/daemon.toml` on Linux.
- A minimal configuration sets `lif_count`, `izh_count`, `channels`, `tick_rate_hz`, `log_level`, `spine_sub_port`, `spine_pub_port`, and `model_path` (`model_path` is unused by the stub backend). With the stub backend it runs a headless SNN tick loop and logs `🔌 Using stub backend`.
