{
  lib,
  stdenv,
  rustPlatform,
  makeWrapper,
  git,
  libnotify,
  coreutils-prefixed,
  procps,
  darwin,
  # A fallback on PATH only: the Herdr client must match the server, so a
  # `herdr` the user already runs wins. null leaves it out.
  herdr ? null,
}:

let
  cargoToml = lib.importTOML ../Cargo.toml;
  bundleHerdr = herdr != null && lib.meta.availableOn stdenv.hostPlatform herdr;
in
rustPlatform.buildRustPackage {
  pname = cargoToml.package.name;
  inherit (cargoToml.package) version;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../rustfmt.toml
      ../src
      (lib.fileset.fileFilter (file: file.hasExt "rs") ../tests)
    ];
  };

  # Every crate is pinned by its checksum in Cargo.lock: no vendor hash to update.
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ makeWrapper ];

  # checkPhase runs `cargo test`: the unit tests (screen snapshots
  # included) and tests/switching.rs, which runs the built itm with the
  # test binary playing herdr and git. git: local work and URL rewriting;
  # ps: the process table read the way macOS and the BSDs read it.
  nativeCheckInputs = [
    git
    (if stdenv.hostPlatform.isDarwin then darwin.ps else procps)
  ];

  # wrapProgram keeps argv[0] (…/bin/itm), which the attach loop's
  # process-name rule needs (DESIGN.md section 2). git for projects, gdate
  # for snooze phrases itm does not read itself, notify-send on Linux;
  # ITM_SELF makes a clone child run this wrapper, with this PATH.
  postInstall = ''
    wrapProgram $out/bin/itm \
      --prefix PATH : ${
        lib.makeBinPath (
          [
            git
            coreutils-prefixed
          ]
          ++ lib.optional stdenv.hostPlatform.isLinux libnotify
        )
      } \
      --set ITM_SELF $out/bin/itm \
      ${lib.optionalString bundleHerdr "--suffix PATH : ${lib.makeBinPath [ herdr ]}"}
  '';

  meta = {
    inherit (cargoToml.package) description;
    license = lib.licenses.mit;
    platforms = lib.platforms.unix;
    mainProgram = "itm";
  };
}
