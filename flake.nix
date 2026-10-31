{
  description = "fedimix - a Fedimint-based bitcoin mixer (GTK4 desktop app)";

  nixConfig = {
    extra-substituters = [ "https://fedimint.cachix.org" ];
    extra-trusted-public-keys = [
      "fedimint.cachix.org-1:FpJJjy1iPVlvyv4OMiN5y9+/arFLPcnZhZVVCHCDYTs="
    ];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    # No `follows`: its binaries must come from its own nixpkgs to match the cachix cache.
    fedimint.url = "github:fedimint/fedimint/v0.13.0";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      fenix,
      crane,
      fedimint,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        toolchain = fenix.packages.${system}.stable.withComponents [
          "cargo"
          "clippy"
          "rust-src"
          "rustc"
          "rustfmt"
          "rust-analyzer"
        ];

        craneLib = (crane.mkLib pkgs).overrideToolchain toolchain;

        libclangPath = "${pkgs.llvmPackages.libclang.lib}/lib";

        nativeBuildInputs = with pkgs; [
          pkg-config
          wrapGAppsHook4
          # fedimint-rocksdb builds librocksdb from source
          clang
          cmake
          protobuf
        ];

        buildInputs = with pkgs; [
          glib
          pango
          gtk4
          libadwaita
          # async-hwi talks to hardware wallets through hidapi and libudev
          udev
          hidapi
        ];

        # cleanCargoSource keeps only Cargo and Rust files, but the GTK crate
        # embeds style.css via include_str!. Every target shares this source.
        src = pkgs.lib.cleanSourceWith {
          src = ./.;
          name = "source";
          filter =
            path: type: craneLib.filterCargoSources path type || builtins.match ".*\\.css$" path != null;
        };

        commonArgs = {
          inherit src;
          pname = "fedimix";
          strictDeps = true;
          inherit nativeBuildInputs buildInputs;
          LIBCLANG_PATH = libclangPath;
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        fedimix = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            # Test binaries are not wrapped; they load GTK from here.
            preCheck = ''
              export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath (buildInputs ++ [ pkgs.stdenv.cc.cc.lib ])}
            '';
            # Rust links without a runpath, so the binary would only start
            # inside the dev shell.
            postInstall = ''
              patchelf --add-rpath ${pkgs.lib.makeLibraryPath (buildInputs ++ [ pkgs.stdenv.cc.cc.lib ])} $out/bin/fedimix
            '';
            # A bad flag exits 2 before touching a display or a data folder; 127
            # means libraries are missing.
            doInstallCheck = true;
            installCheckPhase = ''
              runHook preInstallCheck
              status=0
              $out/bin/fedimix --install-check || status=$?
              [ "$status" -eq 2 ]
              runHook postInstallCheck
            '';
            meta.mainProgram = "fedimix";
          }
        );
      in
      {
        packages.default = fedimix;

        checks = {
          inherit fedimix;
          clippy = craneLib.cargoClippy (
            commonArgs
            // {
              inherit cargoArtifacts;
              cargoClippyExtraArgs = "--all-targets -- -D warnings";
            }
          );
          fmt = craneLib.cargoFmt { inherit src; inherit (commonArgs) pname; };
        };

        devShells = rec {
          default = pkgs.mkShell {
            inherit nativeBuildInputs buildInputs;
            packages = [
              toolchain
              pkgs.just
              pkgs.sway
              pkgs.grim
              pkgs.jq
              pkgs.dbus
            ];
            LIBCLANG_PATH = libclangPath;
            # The built binary needs the GTK/libadwaita libraries (and
            # librocksdb's libstdc++) at runtime.
            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath (buildInputs ++ [ pkgs.stdenv.cc.cc.lib ]);
            # Lets GTK find schemas/icons when running via `cargo run`.
            shellHook = ''
              export XDG_DATA_DIRS="${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk4}/share/gsettings-schemas/${pkgs.gtk4.name}:${pkgs.adwaita-icon-theme}/share:$XDG_DATA_DIRS"
            '';
          };

          # Adds devimint and the daemons it spawns. Separate so that plain
          # `nix develop` never fetches or builds fedimint.
          integration =
            let
              # fedimint's own nixpkgs and overlay, so that bitcoind, lnd and
              # esplora match what its cachix cache holds
              fmPkgs = import fedimint.inputs.nixpkgs {
                inherit system;
                overlays = [ fedimint.overlays.all ];
              };
              fm = fedimint.packages.${system};
            in
            pkgs.mkShell {
              inputsFrom = [ default ];
              packages = [
                fm.devimint
                fm.fedimintd
                fm.fedimint-cli
                fm.gatewayd
                fm.gateway-cli
                fm.fedimint-dbtool
                # devimint's dev-fed dies with "Could not spawn: recurringd" without these
                fm.fedimint-recurringd
                fm.fedimint-recurringdv2
                fmPkgs.bitcoind
                fmPkgs.lnd
                fmPkgs.esplora-electrs
                fmPkgs.jq
                fmPkgs.perl
                fmPkgs.procps
                fmPkgs.netcat
                fmPkgs.which
              ];
              FM_ESPLORA_BASE_EXECUTABLE = "${fmPkgs.esplora-electrs}/bin/esplora";
              # (inputsFrom merges the default shellHook, but not env vars.)
              inherit (default) LIBCLANG_PATH;
              # librocksdb needs libstdc++ at test time. The script sets it for
              # the `cargo test` step only, so devimint's daemons, built from
              # another nixpkgs, never load it.
              FEDIMIX_TEST_LD_LIBRARY_PATH = "${pkgs.stdenv.cc.cc.lib}/lib";
              # The same for the app itself, in `scripts/test-real-ui.sh`.
              FEDIMIX_UI_LD_LIBRARY_PATH = default.LD_LIBRARY_PATH;
            };
        };
      }
    );
}
