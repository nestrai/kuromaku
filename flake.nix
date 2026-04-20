{
  description = "koto - reproducible AI agent teams";

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
    in
    {
      devShells = forAllSystems (system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
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
