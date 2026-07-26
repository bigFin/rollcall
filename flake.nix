{
  description = "SSH-native control plane for coding-agent sessions";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };

        rollcall = pkgs.rustPlatform.buildRustPackage {
          pname = "rollcall";
          version = "0.1.0";
          src = self;

          cargoLock.lockFile = ./Cargo.lock;

          meta = {
            description = "SSH-native control plane for coding-agent sessions";
            mainProgram = "rollcall";
            platforms = pkgs.lib.platforms.unix;
          };
        };
      in
      {
        formatter = pkgs.nixfmt;

        packages = {
          inherit rollcall;
          default = rollcall;
        };

        apps.default = {
          type = "app";
          program = "${rollcall}/bin/rollcall";
          meta.description = "Open the rollcall coding-agent session control plane";
        };

        checks.default = rollcall;

        devShells.default = pkgs.mkShell {
          inputsFrom = [ rollcall ];

          packages = with pkgs; [
            cargo
            clippy
            nixfmt
            rust-analyzer
            rustc
            rustfmt
            sqlite
          ];

          env = {
            RUST_BACKTRACE = "1";
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          };
        };
      }
    );
}
