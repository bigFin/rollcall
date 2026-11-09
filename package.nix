{
  lib,
  rustPlatform,
  pkg-config,
  sqlite,
  makeWrapper,
  bash,
  python3,
  nodejs,
}:
rustPlatform.buildRustPackage {
  pname = "rollcall";
  version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
  src = ./.;

  cargoLock.lockFile = ./Cargo.lock;
  nativeBuildInputs = [
    makeWrapper
    pkg-config
  ];
  buildInputs = [ sqlite ];
  nativeCheckInputs = [
    bash
    python3
    nodejs
  ];
  postInstall = ''
    install -Dm644 integrations/pi/rollcall.ts $out/share/rollcall/pi/rollcall.ts
  '';
  postFixup = ''
    wrapProgram $out/bin/rollcall --prefix PATH : ${lib.makeBinPath [ python3 ]}
  '';

  meta = {
    description = "SSH-native control plane for coding-agent sessions";
    mainProgram = "rollcall";
    platforms = lib.platforms.unix;
  };
}
