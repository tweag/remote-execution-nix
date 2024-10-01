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
      rec {
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
          #src = ./.;
          dontUnpack = true;


          installPhase = ''
            mkdir -p $out
            echo ${builtins.toString builtins.currentTime} >> $out/out.txt
            echo "${pkgs.bash}" >> $out/hi.txt
            mkdir -p $out/subdir/again
            echo "hi" >> $out/subdir/again/file.txt
          '';
        };

        packages.test-file = pkgs.stdenv.mkDerivation {
          pname = "test-file";
          version = "0.0.1";
          src = ./.;

          installPhase = ''
            cat ${self.packages.x86_64-linux.test}/hi.txt
            echo "Hello" >&2
            echo ${builtins.toString builtins.currentTime} >> $out
            echo "World" >&2
            chmod +x $out
            echo "on stdout"
          '';
        };

        packages.test-slow = pkgs.stdenv.mkDerivation {
          pname = "test-slow";
          version = "0.0.1";
          #src = ./.;
          dontUnpack = true;

          installPhase = ''
            echo "Hello" >&2
            echo "World" >&2
            echo not the current time > $out
            chmod +x $out
            echo "on stdout and sleeping"
            sleep 5
            echo "Done sleeping"
          '';
        };

        packages.test-deps = pkgs.stdenv.mkDerivation {
          pname = "test-deps";
          version = "0.0.1";
          dontUnpack = true;

          installPhase = ''
            mkdir -p "$out"
            echo 'hello' > $out/hello.txt
          '';

        };

        
        packages.test-depends = pkgs.stdenv.mkDerivation {
          pname = "test-depends";
          version = "0.0.1";
          dontUnpack = true;

          installPhase = ''
            mkdir -p "$out"
            cat "${packages.test-deps}/hello.txt" > $out/hello.txt
            echo ' deps' >> $out/hello.txt
          '';
        };
      }
    );
}
