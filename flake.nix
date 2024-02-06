{
  description = "A basic rust devshell";

  inputs = {
    nixpkgs.url      = "github:NixOS/nixpkgs/nixos-unstable";
    fenix.url = "github:nix-community/fenix";
    flake-utils.url  = "github:numtide/flake-utils";
    unstable.url = "nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs, fenix, flake-utils, unstable, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ fenix.overlays.default ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };
        rust-toolchain = pkgs.fenix.combine [
          pkgs.fenix.stable.defaultToolchain
          pkgs.fenix.stable.rust-src
          pkgs.fenix.stable.rust-analyzer
        ];
      in
      with pkgs;
      {
        devShells.default = mkShell {
          buildInputs = [
            rust-toolchain
            cargo-expand
            protobuf
            grpcurl
          ];
          
          shellHook = ''
            alias ls=exa
            alias find=fd
          '';
        };
      }
    );
}
