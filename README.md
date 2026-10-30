# Fedimix

Fedimix is a Linux-first desktop application, written in Rust with GTK4 and
libadwaita, that will become a Fedimint-based bitcoin mixer. It is at the
scaffolding stage: the app currently opens a window with a placeholder page
and contains no mixer logic yet.

Development uses Nix. `nix develop` (or `direnv allow`, via the bundled
`.envrc`) gives you a shell with the stable Rust toolchain (including clippy,
rustfmt and rust-analyzer), GTK4, libadwaita and `just`. From there:

```sh
nix develop
just run      # build and launch the app
just clippy   # lint with warnings denied
just fmt      # format the code
```

`nix build` produces a wrapped release binary at `result/bin/fedimix`.
