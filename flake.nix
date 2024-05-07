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
          pkgs.fenix.targets.x86_64-unknown-linux-musl.stable.rust-std
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

        packages.test = pkgs.stdenv.mkDerivation {
          pname = "test";
          version = "0.0.1";
          src = ./.;


          installPhase = ''
            mkdir -p $out
            echo ${builtins.toString builtins.currentTime} >> $out/out.txt
            echo "${pkgs.bash}" >> $out/hi.txt
            mkdir -p $out/subdir/again
            echo "hi" >> $out/subdir/again/file.txt
          '';
        };
      }
    );
}
