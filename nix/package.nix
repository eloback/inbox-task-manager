{
  lib,
  stdenv,
  python3Packages,
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
  pyproject = lib.importTOML ../pyproject.toml;
  bundleHerdr = herdr != null && lib.meta.availableOn stdenv.hostPlatform herdr;
in
python3Packages.buildPythonApplication {
  pname = pyproject.project.name;
  inherit (pyproject.project) version;
  pyproject = true;

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../pyproject.toml
      ../README.md
      ../LICENSE
      (lib.fileset.fileFilter (file: file.hasExt "py") ../inbox_task_manager)
      (lib.fileset.fileFilter (file: file.hasExt "py") ../tests)
    ];
  };

  build-system = [ python3Packages.setuptools ];
  dependencies = [ python3Packages.textual ];

  # ps: the tests read the process table the way macOS and the BSDs do too.
  nativeCheckInputs = [
    git
    (if stdenv.hostPlatform.isDarwin then darwin.ps else procps)
  ];
  # buildPythonApplication runs this after installing; doCheck switches it on.
  installCheckPhase = ''
    runHook preInstallCheck
    python3 -m unittest discover -s tests
    runHook postInstallCheck
  '';
  doCheck = true;

  makeWrapperArgs = [
    # notify-send on Linux; gdate for snooze phrases itm does not read itself.
    "--prefix"
    "PATH"
    ":"
    (lib.makeBinPath (
      [
        git
        coreutils-prefixed
      ]
      ++ lib.optional stdenv.hostPlatform.isLinux libnotify
    ))
    "--set"
    "ITM_SELF"
    "${placeholder "out"}/bin/inbox-task-manager"
  ]
  ++ lib.optionals bundleHerdr [
    "--suffix"
    "PATH"
    ":"
    (lib.makeBinPath [ herdr ])
  ];

  pythonImportsCheck = [
    "inbox_task_manager"
    "inbox_task_manager.tui"
  ];

  meta = {
    inherit (pyproject.project) description;
    license = lib.licenses.mit;
    platforms = lib.platforms.unix;
    # The attach loop's command line must name itm (DESIGN.md, process name).
    mainProgram = "inbox-task-manager";
  };
}
