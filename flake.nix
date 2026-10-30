{
  description = "fedimix - a Fedimint-based bitcoin mixer (GTK4 desktop app)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      fenix,
      crane,
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

        nativeBuildInputs = with pkgs; [
          pkg-config
          wrapGAppsHook4
        ];

        buildInputs = with pkgs; [
          glib
          pango
          gtk4
          libadwaita
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
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        fedimix = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
            # Test binaries are not wrapped; they load GTK from here.
            preCheck = ''
              export LD_LIBRARY_PATH=${pkgs.lib.makeLibraryPath buildInputs}
            '';
            # Rust links without a runpath, so the binary would only start
            # inside the dev shell.
            postInstall = ''
              patchelf --add-rpath ${pkgs.lib.makeLibraryPath buildInputs} $out/bin/fedimix
            '';
            # Exits 2 before touching a display; 127 means libraries are missing.
            doInstallCheck = true;
            installCheckPhase = ''
              runHook preInstallCheck
              status=0
              $out/bin/fedimix || status=$?
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

        devShells.default = pkgs.mkShell {
          inherit nativeBuildInputs buildInputs;
          packages = [
            toolchain
            pkgs.just
            pkgs.sway
            pkgs.grim
            pkgs.jq
            pkgs.dbus
          ];
          # The built binary needs the GTK/libadwaita libraries at runtime.
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath buildInputs;
          # Lets GTK find schemas/icons when running via `cargo run`.
          shellHook = ''
            export XDG_DATA_DIRS="${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk4}/share/gsettings-schemas/${pkgs.gtk4.name}:${pkgs.adwaita-icon-theme}/share:$XDG_DATA_DIRS"
          '';
        };
      }
    );
}
