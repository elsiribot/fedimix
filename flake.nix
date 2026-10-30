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
          gtk4
          libadwaita
        ];

        commonArgs = {
          src = craneLib.cleanCargoSource ./.;
          strictDeps = true;
          inherit nativeBuildInputs buildInputs;
        };

        cargoArtifacts = craneLib.buildDepsOnly commonArgs;

        fedimix = craneLib.buildPackage (
          commonArgs
          // {
            inherit cargoArtifacts;
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
          fmt = craneLib.cargoFmt { src = commonArgs.src; };
        };

        devShells.default = pkgs.mkShell {
          inherit nativeBuildInputs buildInputs;
          packages = [
            toolchain
            pkgs.just
          ];
          # Lets GTK find schemas/icons when running via `cargo run`.
          shellHook = ''
            export XDG_DATA_DIRS="${pkgs.gsettings-desktop-schemas}/share/gsettings-schemas/${pkgs.gsettings-desktop-schemas.name}:${pkgs.gtk4}/share/gsettings-schemas/${pkgs.gtk4.name}:${pkgs.adwaita-icon-theme}/share:$XDG_DATA_DIRS"
          '';
        };
      }
    );
}
