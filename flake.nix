{
  description = "SSH-native control plane for coding-agent sessions";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      nixpkgs,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };

        rollcall = pkgs.callPackage ./package.nix { };
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
          packages = with pkgs; [
            bash
            cargo
            clippy
            nixfmt
            nodejs
            openssh
            python3
            rust-analyzer
            rustc
            rustfmt
            sqlite
            tmux
          ];

          env = {
            RUST_BACKTRACE = "1";
            RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
          };
        };
      }
    );
}
