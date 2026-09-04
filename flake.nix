{
  description = "kuromaku - reproducible AI agent teams";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = pkgsFor system;
        in
        rec {
          kuro = pkgs.rustPlatform.buildRustPackage {
            pname = "kuromaku";
            # Single source of truth: Cargo.toml. Read at eval time so the
            # nix package version cannot drift from the crate version
            # (#398); `just check-version` guards the remaining seams.
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
            src = ./.;
            cargoLock = {
              lockFile = ./Cargo.lock;
            };
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ pkgs.openssl ];
            doCheck = false;
          };
          default = kuro;
        }
      );

      devShells = forAllSystems (system:
        let
          pkgs = pkgsFor system;
          rust = pkgs.rust-bin.fromRustupToolchain {
            channel = "stable";
            components = [ "clippy" "rustfmt" "rust-analyzer" ];
          };
        in
        {
          default = pkgs.mkShell {
            buildInputs = [
              rust
              pkgs.just
              pkgs.cargo-dist
              pkgs.cargo-watch
            ];
          };
        }
      );
    };
}
